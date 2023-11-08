use super::{JsonRpcParams, LooseId, SingleRequest};
use crate::{
    app::App,
    block_number::CacheMode,
    errors::Web3ProxyResult,
    frontend::{
        authorization::{key_is_authorized, Authorization, RequestOrMethod, ResponseOrBytes},
        rpc_proxy_ws::ProxyMode,
    },
    globals::APP,
    response_cache::JsonRpcQueryCacheKey,
    rpcs::{blockchain::Web3ProxyBlock, one::Web3Rpc},
    secrets::RpcSecretKey,
    stats::{AppStat, BackendRequests},
};
use anyhow::Context;
use axum::headers::{Origin, Referer, UserAgent};
use chrono::Utc;
use derivative::Derivative;
use ethers::types::U64;
use rust_decimal::Decimal;
use serde::{ser::SerializeStruct, Serialize};
use serde_json::{json, value::RawValue};
use std::{borrow::Cow, sync::Arc};
use std::{
    fmt::{self, Display},
    net::IpAddr,
};
use std::{
    mem,
    sync::atomic::{self, AtomicBool, AtomicI64, AtomicU64},
    time::Duration,
};
use tokio::{
    sync::{mpsc, OwnedSemaphorePermit},
    time::Instant,
};
use tracing::{error, trace};

#[cfg(feature = "rdkafka")]
use {
    crate::{jsonrpc, kafka::KafkaDebugLogger},
    tracing::warn,
    ulid::Ulid,
};

#[derive(Derivative)]
#[derivative(Default)]
pub struct RequestBuilder {
    app: Option<Arc<App>>,
    archive_request: bool,
    head_block: Option<Web3ProxyBlock>,
    authorization: Option<Arc<Authorization>>,
    request_or_method: RequestOrMethod,
}

impl RequestBuilder {
    pub fn new(app: Arc<App>) -> Self {
        let head_block = app.head_block_receiver().borrow().clone();

        Self {
            app: Some(app),
            head_block,
            ..Default::default()
        }
    }

    pub fn authorize_internal(self) -> Web3ProxyResult<Self> {
        // TODO: allow passing proxy_mode to internal?
        let authorization = Authorization::internal()?;

        Ok(Self {
            authorization: Some(Arc::new(authorization)),
            ..self
        })
    }

    pub async fn authorize_premium(
        self,
        ip: &IpAddr,
        rpc_key: &RpcSecretKey,
        origin: Option<&Origin>,
        user_agent: Option<&UserAgent>,
        proxy_mode: ProxyMode,
        referer: Option<&Referer>,
    ) -> Web3ProxyResult<Self> {
        let app = self
            .app
            .as_ref()
            .context("app is required for public requests")?;

        // TODO: we can check authorization now, but the semaphore needs to wait!
        let authorization =
            key_is_authorized(app, rpc_key, ip, origin, proxy_mode, referer, user_agent).await?;

        Ok(Self {
            authorization: Some(Arc::new(authorization)),
            ..self
        })
    }

    /// TODO: this takes a lot more things
    pub fn authorize_public(
        self,
        ip: &IpAddr,
        origin: Option<&Origin>,
        user_agent: Option<&UserAgent>,
        proxy_mode: ProxyMode,
        referer: Option<&Referer>,
    ) -> Web3ProxyResult<Self> {
        let app = self
            .app
            .as_ref()
            .context("app is required for public requests")?;

        let authorization = Authorization::external(
            &app.config.allowed_origin_requests_per_period,
            ip,
            origin,
            proxy_mode,
            referer,
            user_agent,
        )?;

        let head_block = app.watch_consensus_head_receiver.borrow().clone();

        Ok(Self {
            authorization: Some(Arc::new(authorization)),
            head_block,
            ..self
        })
    }

    pub fn set_archive_request(self, archive_request: bool) -> Self {
        Self {
            archive_request,
            ..self
        }
    }

    /// replace 'latest' in the json and figure out the minimum and maximum blocks.
    /// also tarpit invalid methods.
    pub async fn set_request(self, request: SingleRequest) -> Web3ProxyResult<Self> {
        Ok(Self {
            request_or_method: RequestOrMethod::Request(request),
            ..self
        })
    }

    pub fn set_method(self, method: Cow<'static, str>, size: usize) -> Self {
        Self {
            request_or_method: RequestOrMethod::Method(method, size),
            ..self
        }
    }

    pub async fn build(
        &self,
        max_wait: Option<Duration>,
    ) -> Web3ProxyResult<Arc<ValidatedRequest>> {
        // TODO: make this work without app being set?
        let app = self.app.as_ref().context("app is required")?;

        let authorization = self
            .authorization
            .clone()
            .context("authorization is required")?;

        let permit = authorization.permit(app).await?;

        let x = ValidatedRequest::new_with_app(
            app,
            authorization,
            max_wait,
            permit,
            self.request_or_method.clone(),
            self.head_block.clone(),
        )
        .await;

        if let Ok(x) = &x {
            if self.archive_request {
                x.archive_request.store(true, atomic::Ordering::Release);
            }
        }

        // todo!(anything else to set?)

        x
    }
}

/// TODO:
/// TODO: instead of a bunch of atomics, this should probably use a RwLock. need to think more about how parallel requests are going to work though
#[derive(Debug, Derivative)]
#[derivative(Default)]
pub struct ValidatedRequest {
    /// TODO: set archive_request during the new instead of after
    /// TODO: this is more complex than "requires a block older than X height". different types of data can be pruned differently
    pub archive_request: AtomicBool,

    pub authorization: Arc<Authorization>,

    pub cache_mode: CacheMode,

    /// TODO: this should probably be in a global config. although maybe if we run multiple chains in one process this will be useful
    pub chain_id: u64,

    pub head_block: Option<Web3ProxyBlock>,

    /// TODO: this should be in a global config. not copied to every single request
    pub usd_per_cu: Decimal,

    pub inner: RequestOrMethod,

    /// Instant that the request was received (or at least close to it)
    /// We use Instant and not timestamps to avoid problems with leap seconds and similar issues
    #[derivative(Default(value = "Instant::now()"))]
    pub start_instant: Instant,
    /// if this is empty, there was a cache_hit
    /// otherwise, it is populated with any rpc servers that were used by this request
    pub backend_requests: BackendRequests,
    /// The number of times the request got stuck waiting because no servers were synced
    pub no_servers: AtomicU64,
    /// If handling the request hit an application error
    /// This does not count things like a transcation reverting or a malformed request
    pub error_response: AtomicBool,
    /// Size in bytes of the JSON response. Does not include headers or things like that.
    pub response_bytes: AtomicU64,
    /// How many milliseconds it took to respond to the request
    pub response_millis: AtomicU64,
    /// What time the (first) response was proxied.
    /// TODO: think about how to store response times for ProxyMode::Versus
    pub response_timestamp: AtomicI64,
    /// True if the response required querying a backup RPC
    /// RPC aggregators that query multiple providers to compare response may use this header to ignore our response.
    pub response_from_backup_rpc: AtomicBool,
    /// If the request is invalid or received a jsonrpc error response (excluding reverts)
    pub user_error_response: AtomicBool,

    #[cfg(feature = "rdkafka")]
    /// ProxyMode::Debug logs requests and responses with Kafka
    /// TODO: maybe this shouldn't be determined by ProxyMode. A request param should probably enable this
    pub kafka_debug_logger: Option<Arc<KafkaDebugLogger>>,

    #[cfg(not(feature = "rdkafka"))]
    pub kafka_debug_logger: Option<()>,

    /// Cancel-safe channel for sending stats to the buffer
    pub stat_sender: Option<mpsc::UnboundedSender<AppStat>>,

    /// How long to spend waiting for an rpc that can serve this request
    pub connect_timeout: Duration,
    /// How long to spend waiting for an rpc to respond to this request
    /// TODO: this should start once the connection is established
    pub expire_timeout: Duration,

    /// limit the number of concurrent requests from a given user.
    pub permit: Option<OwnedSemaphorePermit>,
}

impl Display for ValidatedRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}({})",
            self.inner.method(),
            serde_json::to_string(self.inner.params()).expect("this should always serialize")
        )
    }
}

impl Serialize for ValidatedRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("request", 7)?;

        state.serialize_field(
            "archive_request",
            &self.archive_request.load(atomic::Ordering::Acquire),
        )?;

        state.serialize_field("chain_id", &self.chain_id)?;

        state.serialize_field("head_block", &self.head_block)?;
        state.serialize_field("request", &self.inner)?;

        state.serialize_field("elapsed", &self.start_instant.elapsed().as_secs_f32())?;

        {
            let backend_names = self.backend_requests.lock();

            let backend_names = backend_names
                .iter()
                .map(|x| x.name.as_str())
                .collect::<Vec<_>>();

            state.serialize_field("backend_requests", &backend_names)?;
        }

        state.serialize_field(
            "response_bytes",
            &self.response_bytes.load(atomic::Ordering::Acquire),
        )?;

        state.end()
    }
}

/// TODO: move all of this onto RequestBuilder
impl ValidatedRequest {
    #[allow(clippy::too_many_arguments)]
    async fn new_with_options(
        app: Option<&App>,
        authorization: Arc<Authorization>,
        chain_id: u64,
        mut head_block: Option<Web3ProxyBlock>,
        #[cfg(feature = "rdkafka")] kafka_debug_logger: Option<Arc<KafkaDebugLogger>>,
        max_wait: Option<Duration>,
        permit: Option<OwnedSemaphorePermit>,
        mut request: RequestOrMethod,
        usd_per_cu: Decimal,
    ) -> Web3ProxyResult<Arc<Self>> {
        let start_instant = Instant::now();

        let stat_sender = app.and_then(|x| x.stat_sender.clone());

        // let request: RequestOrMethod = request.into();

        // we VERY INTENTIONALLY log to kafka BEFORE calculating the cache key
        // this is because calculating the cache_key may modify the params!
        // for example, if the request specifies "latest" as the block number, we replace it with the actual latest block number
        #[cfg(feature = "rdkafka")]
        if let Some(ref kafka_debug_logger) = kafka_debug_logger {
            // TODO: channels might be more ergonomic than spawned futures
            // spawned things run in parallel easier but generally need more Arcs
            kafka_debug_logger.log_debug_request(&request);
        }
        #[cfg(not(feature = "rdkafka"))]
        let kafka_debug_logger = None;

        if head_block.is_none() {
            if let Some(app) = app {
                head_block = app.head_block_receiver().borrow().clone();
            }
        }

        // now that kafka has logged the user's original params, we can calculate the cache key
        // calculating the cache key might alter the params

        // TODO: modify CacheMode::new to wait for a future block if one is requested! be sure to update head_block too!
        let cache_mode = match &mut request {
            RequestOrMethod::Request(x) => CacheMode::new(x, head_block.as_ref(), app).await?,
            _ => CacheMode::Never,
        };

        // TODO: what should we do if we want a really short max_wait?
        let connect_timeout = Duration::from_secs(10);

        let expire_timeout = if let Some(max_wait) = max_wait {
            max_wait
        } else if authorization.active_premium().await {
            Duration::from_secs(295)
        } else {
            Duration::from_secs(60)
        }
        .max(connect_timeout);

        let x = Self {
            archive_request: false.into(),
            authorization,
            backend_requests: Default::default(),
            cache_mode,
            chain_id,
            error_response: false.into(),
            connect_timeout,
            expire_timeout,
            head_block: head_block.clone(),
            kafka_debug_logger,
            no_servers: 0.into(),
            inner: request,
            permit,
            response_bytes: 0.into(),
            response_from_backup_rpc: false.into(),
            response_millis: 0.into(),
            response_timestamp: 0.into(),
            start_instant,
            stat_sender,
            usd_per_cu,
            user_error_response: false.into(),
        };

        Ok(Arc::new(x))
    }

    /// todo!(this shouldn't be public. use the RequestBuilder)
    pub async fn new_with_app(
        app: &App,
        authorization: Arc<Authorization>,
        max_wait: Option<Duration>,
        permit: Option<OwnedSemaphorePermit>,
        request: RequestOrMethod,
        head_block: Option<Web3ProxyBlock>,
    ) -> Web3ProxyResult<Arc<Self>> {
        #[cfg(feature = "rdkafka")]
        let kafka_debug_logger = if matches!(authorization.checks.proxy_mode, ProxyMode::Debug) {
            // TODO: get this out of tracing instead (where we have a String from Amazon's LB)
            let request_ulid = Ulid::new();

            KafkaDebugLogger::try_new(
                app,
                authorization.clone(),
                head_block.as_ref().map(|x| x.number()),
                "web3_proxy:rpc",
                request_ulid,
            )
        } else {
            None
        };

        let chain_id = app.config.chain_id;

        let usd_per_cu = app.config.usd_per_cu.unwrap_or_default();

        Self::new_with_options(
            Some(app),
            authorization,
            chain_id,
            head_block,
            #[cfg(feature = "rdkafka")]
            kafka_debug_logger,
            max_wait,
            permit,
            request,
            usd_per_cu,
        )
        .await
    }

    pub async fn new_internal<P: JsonRpcParams>(
        method: Cow<'static, str>,
        params: &P,
        head_block: Option<Web3ProxyBlock>,
        max_wait: Option<Duration>,
    ) -> Web3ProxyResult<Arc<Self>> {
        let authorization = Arc::new(Authorization::internal().unwrap());

        // todo!(we need a real id! increment a counter on the app or websocket-only providers are going to have a problem)
        let id = LooseId::Number(1);

        // TODO: this seems inefficient
        let request = SingleRequest::new(id, method, json!(params)).unwrap();

        if let Some(app) = APP.get() {
            Self::new_with_app(
                app,
                authorization,
                max_wait,
                None,
                request.into(),
                head_block,
            )
            .await
        } else {
            Self::new_with_options(
                None,
                authorization,
                0,
                head_block,
                #[cfg(feature = "rdkafka")]
                None,
                max_wait,
                None,
                request.into(),
                Default::default(),
            )
            .await
        }
    }

    #[inline]
    pub fn backend_rpcs_used(&self) -> Vec<Arc<Web3Rpc>> {
        self.backend_requests.lock().clone()
    }

    pub fn cache_key(&self) -> Option<u64> {
        match &self.cache_mode {
            CacheMode::Never => None,
            x => {
                let x = JsonRpcQueryCacheKey::new(x, &self.inner).hash();

                Some(x)
            }
        }
    }

    #[inline]
    pub fn cache_jsonrpc_errors(&self) -> bool {
        self.cache_mode.cache_jsonrpc_errors()
    }

    #[inline]
    pub fn id(&self) -> Box<RawValue> {
        self.inner.id()
    }

    #[inline]
    pub fn max_block_needed(&self) -> Option<U64> {
        if let Some(to_block) = self.cache_mode.to_block() {
            Some(to_block.num())
        } else {
            self.head_block
                .as_ref()
                .map(|head_block| head_block.number())
        }
    }

    #[inline]
    pub fn min_block_needed(&self) -> Option<U64> {
        if self.archive_request.load(atomic::Ordering::Acquire) {
            Some(U64::zero())
        } else {
            self.cache_mode.from_block().map(|x| x.num())
        }
    }

    #[inline]
    pub fn connect_timeout_at(&self) -> Instant {
        self.start_instant + self.connect_timeout
    }

    #[inline]
    pub fn connect_timeout(&self) -> bool {
        self.connect_timeout_at() <= Instant::now()
    }

    #[inline]
    pub fn expire_at(&self) -> Instant {
        // TODO: get from config
        // erigon's timeout is 5 minutes so we want it shorter than that
        self.start_instant + self.expire_timeout
    }

    #[inline]
    pub fn expired(&self) -> bool {
        self.expire_at() <= Instant::now()
    }

    pub fn try_send_stat(mut self) -> Web3ProxyResult<()> {
        if let Some(stat_sender) = self.stat_sender.take() {
            trace!(?self, "sending stat");

            let stat: AppStat = self.into();

            if let Err(err) = stat_sender.send(stat) {
                error!(?err, "failed sending stat");
                // TODO: return it? that seems like it might cause an infinite loop
                // TODO: but dropping stats is bad... hmm... i guess better to undercharge customers than overcharge
            };

            trace!("stat sent successfully");
        }

        Ok(())
    }

    pub fn add_response<'a, R: Into<ResponseOrBytes<'a>>>(&'a self, response: R) {
        // TODO: fetch? set? should it be None in a Mutex? or a OnceCell?
        let response = response.into();

        let num_bytes = response.num_bytes();

        self.response_bytes
            .fetch_add(num_bytes, atomic::Ordering::AcqRel);

        self.response_millis.fetch_add(
            self.start_instant.elapsed().as_millis() as u64,
            atomic::Ordering::AcqRel,
        );

        // TODO: record first or last timestamp? really, we need multiple
        self.response_timestamp
            .store(Utc::now().timestamp(), atomic::Ordering::Release);

        // TODO: set user_error_response and error_response here instead of outside this function

        #[cfg(feature = "rdkafka")]
        if let Some(kafka_debug_logger) = self.kafka_debug_logger.as_ref() {
            if let ResponseOrBytes::Response(response) = response {
                match response {
                    jsonrpc::SingleResponse::Parsed(response) => {
                        kafka_debug_logger.log_debug_response(response);
                    }
                    jsonrpc::SingleResponse::Stream(_) => {
                        warn!("need to handle streaming response debug logging");
                    }
                }
            }
        }
    }

    pub fn try_send_arc_stat(self: Arc<Self>) -> Web3ProxyResult<()> {
        match Arc::into_inner(self) {
            Some(x) => x.try_send_stat(),
            None => {
                trace!("could not send stat while other arcs are active");
                Ok(())
            }
        }
    }

    #[inline]
    pub fn proxy_mode(&self) -> ProxyMode {
        self.authorization.checks.proxy_mode
    }

    // TODO: helper function to duplicate? needs to clear request_bytes, and all the atomics tho...
}

impl Drop for ValidatedRequest {
    fn drop(&mut self) {
        if self.stat_sender.is_some() {
            // turn `&mut self` into `self`
            let x = mem::take(self);

            // trace!(?x, "request metadata dropped without stat send");
            let _ = x.try_send_stat();
        }
    }
}

/*
pub fn foo(&self) {
    let payload = payload
        .map_err(|e| Web3ProxyError::from(e).into_response_with_id(None))?
        .0;

    let first_id = payload.first_id();

    let (authorization, _semaphore) = ip_is_authorized(&app, ip, origin, proxy_mode)
        .await
        .map_err(|e| e.into_response_with_id(first_id.clone()))?;

    let authorization = Arc::new(authorization);

    payload
        .tarpit_invalid(&app, &authorization, Duration::from_secs(5))
        .await?;

    // TODO: calculate payload bytes here (before turning into serde_json::Value). that will save serializing later

    // TODO: is first_id the right thing to attach to this error?
    let (status_code, response, rpcs) = app
        .proxy_web3_rpc(authorization, payload)
        .await
        .map_err(|e| e.into_response_with_id(first_id))?;

    let mut response = (status_code, response).into_response();

    // TODO: DRY this up. it is the same code for public and private queries
    let response_headers = response.headers_mut();

    // TODO: this might be slow. think about this more
    // TODO: special string if no rpcs were used (cache hit)?
    let mut backup_used = false;

    let rpcs: String = rpcs
        .into_iter()
        .map(|x| {
            if x.backup {
                backup_used = true;
            }
            x.name.clone()
        })
        .join(",");

    response_headers.insert(
        "X-W3P-BACKEND-RPCS",
        rpcs.parse().expect("W3P-BACKEND-RPCS should always parse"),
    );

    response_headers.insert(
        "X-W3P-BACKUP-RPC",
        backup_used
            .to_string()
            .parse()
            .expect("W3P-BACKEND-RPCS should always parse"),
    );

    Ok(response)
}
 */
