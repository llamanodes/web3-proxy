//! Utilities for authorization of logged in and anonymous users.

use super::rpc_proxy_ws::ProxyMode;
use crate::app::{Web3ProxyApp, APP_USER_AGENT};
use crate::balance::Balance;
use crate::block_number::CacheMode;
use crate::caches::RegisteredUserRateLimitKey;
use crate::errors::{Web3ProxyError, Web3ProxyErrorContext, Web3ProxyResult};
use crate::globals::{global_db_replica_conn, APP};
use crate::jsonrpc::{self, JsonRpcId, JsonRpcParams, JsonRpcRequest};
use crate::response_cache::JsonRpcQueryCacheKey;
use crate::rpcs::blockchain::Web3ProxyBlock;
use crate::rpcs::one::Web3Rpc;
use crate::stats::{AppStat, BackendRequests};
use crate::user_token::UserBearerToken;
use anyhow::Context;
use axum::headers::authorization::Bearer;
use axum::headers::{Header, Origin, Referer, UserAgent};
use chrono::Utc;
use core::fmt;
use deferred_rate_limiter::{DeferredRateLimitResult, DeferredRateLimiter};
use derivative::Derivative;
use derive_more::From;
use entities::{login, rpc_key, user, user_tier};
use ethers::types::{Bytes, U64};
use ethers::utils::keccak256;
use futures::TryFutureExt;
use hashbrown::HashMap;
use http::HeaderValue;
use ipnet::IpNet;
use migration::sea_orm::prelude::Decimal;
use migration::sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use rdkafka::message::{Header as KafkaHeader, OwnedHeaders as KafkaOwnedHeaders, OwnedMessage};
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::util::Timeout as KafkaTimeout;
use redis_rate_limiter::redis::AsyncCommands;
use redis_rate_limiter::{RedisRateLimitResult, RedisRateLimiter};
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::value::RawValue;
use std::borrow::Cow;
use std::fmt::Debug;
use std::fmt::Display;
use std::hash::{Hash, Hasher};
use std::mem;
use std::num::NonZeroU64;
use std::sync::atomic::{self, AtomicBool, AtomicI64, AtomicU64, AtomicUsize};
use std::time::Duration;
use std::{net::IpAddr, str::FromStr, sync::Arc};
use tokio::sync::RwLock as AsyncRwLock;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{error, trace, warn};
use ulid::Ulid;
use uuid::Uuid;

/// This lets us use UUID and ULID while we transition to only ULIDs
/// TODO: custom deserialize that can also go from String to Ulid
#[derive(Copy, Clone, Deserialize)]
pub enum RpcSecretKey {
    Ulid(Ulid),
    Uuid(Uuid),
}

impl RpcSecretKey {
    pub fn new() -> Self {
        Ulid::new().into()
    }

    fn as_128(&self) -> u128 {
        match self {
            Self::Ulid(x) => x.0,
            Self::Uuid(x) => x.as_u128(),
        }
    }
}

impl PartialEq for RpcSecretKey {
    fn eq(&self, other: &Self) -> bool {
        self.as_128() == other.as_128()
    }
}

impl Eq for RpcSecretKey {}

impl Debug for RpcSecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ulid(x) => Debug::fmt(x, f),
            Self::Uuid(x) => {
                let x = Ulid::from(x.as_u128());

                Debug::fmt(&x, f)
            }
        }
    }
}

/// always serialize as a ULID.
impl Serialize for RpcSecretKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Ulid(x) => x.serialize(serializer),
            Self::Uuid(x) => {
                let x: Ulid = x.to_owned().into();

                x.serialize(serializer)
            }
        }
    }
}

/// TODO: should this have IpAddr and Origin or AuthorizationChecks?
#[derive(Debug)]
pub enum RateLimitResult {
    Allowed(Authorization, Option<OwnedSemaphorePermit>),
    RateLimited(
        Authorization,
        /// when their rate limit resets and they can try more requests
        Option<Instant>,
    ),
    /// This key is not in our database. Deny access!
    UnknownKey,
}

#[derive(Clone, Debug)]
pub enum AuthorizationType {
    Internal,
    Frontend,
}

/// TODO: move this
#[derive(Clone, Debug, Default, From)]
pub struct AuthorizationChecks {
    /// database id of the primary user. 0 if anon
    pub user_id: u64,
    /// locally cached balance that may drift slightly if the user is on multiple servers
    pub latest_balance: Arc<AsyncRwLock<Balance>>,
    /// the key used (if any)
    pub rpc_secret_key: Option<RpcSecretKey>,
    /// database id of the rpc key
    /// if this is None, then this request is being rate limited by ip
    pub rpc_secret_key_id: Option<NonZeroU64>,
    /// if None, allow unlimited queries. inherited from the user_tier
    pub max_requests_per_period: Option<u64>,
    // if None, allow unlimited concurrent requests. inherited from the user_tier
    pub max_concurrent_requests: Option<u32>,
    /// if None, allow any Origin
    pub allowed_origins: Option<Vec<Origin>>,
    /// if None, allow any Referer
    pub allowed_referers: Option<Vec<Referer>>,
    /// if None, allow any UserAgent
    pub allowed_user_agents: Option<Vec<UserAgent>>,
    /// if None, allow any IP Address
    pub allowed_ips: Option<Vec<IpNet>>,
    /// Chance to save reverting eth_call, eth_estimateGas, and eth_sendRawTransaction to the database.
    /// depending on the caller, errors might be expected. this keeps us from bloating our database
    /// u16::MAX == 100%
    pub log_revert_chance: u16,
    /// if true, transactions are broadcast only to private mempools.
    /// IMPORTANT! Once confirmed by a miner, they will be public on the blockchain!
    pub private_txs: bool,
    pub proxy_mode: ProxyMode,
    /// if the account had premium when this request metadata was created
    /// they might spend slightly more than they've paid, but we are okay with that
    /// TODO: we could price the request now and if its too high, downgrade. but thats more complex than we need
    pub paid_credits_used: bool,
}

/// TODO: include the authorization checks in this?
#[derive(Clone, Debug)]
pub struct Authorization {
    pub checks: AuthorizationChecks,
    pub ip: IpAddr,
    pub origin: Option<Origin>,
    pub referer: Option<Referer>,
    pub user_agent: Option<UserAgent>,
    pub authorization_type: AuthorizationType,
}

pub struct KafkaDebugLogger {
    topic: String,
    key: Vec<u8>,
    headers: KafkaOwnedHeaders,
    producer: FutureProducer,
    num_requests: AtomicUsize,
    num_responses: AtomicUsize,
}

/// Ulids and Uuids matching the same bits hash the same
impl Hash for RpcSecretKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let x = self.as_128();

        x.hash(state);
    }
}

impl fmt::Debug for KafkaDebugLogger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KafkaDebugLogger")
            .field("topic", &self.topic)
            .finish_non_exhaustive()
    }
}

type KafkaLogResult = Result<(i32, i64), (rdkafka::error::KafkaError, OwnedMessage)>;

impl KafkaDebugLogger {
    fn try_new(
        app: &Web3ProxyApp,
        authorization: Arc<Authorization>,
        head_block_num: Option<U64>,
        kafka_topic: &str,
        request_ulid: Ulid,
    ) -> Option<Arc<Self>> {
        let kafka_producer = app.kafka_producer.clone()?;

        let kafka_topic = kafka_topic.to_string();

        let rpc_secret_key_id = authorization
            .checks
            .rpc_secret_key_id
            .map(|x| x.get())
            .unwrap_or_default();

        let kafka_key =
            rmp_serde::to_vec(&rpc_secret_key_id).expect("ids should always serialize with rmp");

        let chain_id = app.config.chain_id;

        let head_block_num = head_block_num.or_else(|| app.balanced_rpcs.head_block_num());

        // TODO: would be nice to have the block hash too

        // another item is added with the response, so initial_capacity is +1 what is needed here
        let kafka_headers = KafkaOwnedHeaders::new_with_capacity(6)
            .insert(KafkaHeader {
                key: "rpc_secret_key_id",
                value: authorization
                    .checks
                    .rpc_secret_key_id
                    .map(|x| x.to_string())
                    .as_ref(),
            })
            .insert(KafkaHeader {
                key: "ip",
                value: Some(&authorization.ip.to_string()),
            })
            .insert(KafkaHeader {
                key: "request_ulid",
                value: Some(&request_ulid.to_string()),
            })
            .insert(KafkaHeader {
                key: "head_block_num",
                value: head_block_num.map(|x| x.to_string()).as_ref(),
            })
            .insert(KafkaHeader {
                key: "chain_id",
                value: Some(&chain_id.to_le_bytes()),
            });

        // save the key and headers for when we log the response
        let x = Self {
            topic: kafka_topic,
            key: kafka_key,
            headers: kafka_headers,
            producer: kafka_producer,
            num_requests: 0.into(),
            num_responses: 0.into(),
        };

        let x = Arc::new(x);

        Some(x)
    }

    fn background_log(&self, payload: Vec<u8>) -> JoinHandle<KafkaLogResult> {
        let topic = self.topic.clone();
        let key = self.key.clone();
        let producer = self.producer.clone();
        let headers = self.headers.clone();

        let f = async move {
            let record = FutureRecord::to(&topic)
                .key(&key)
                .payload(&payload)
                .headers(headers);

            let produce_future =
                producer.send(record, KafkaTimeout::After(Duration::from_secs(5 * 60)));

            let kafka_response = produce_future.await;

            if let Err((err, msg)) = kafka_response.as_ref() {
                error!("produce kafka request: {} - {:?}", err, msg);
                // TODO: re-queue the msg? log somewhere else like a file on disk?
                // TODO: this is bad and should probably trigger an alarm
            };

            kafka_response
        };

        tokio::spawn(f)
    }

    /// for opt-in debug usage, log the request to kafka
    /// TODO: generic type for request
    pub fn log_debug_request(&self, request: &RequestOrMethod) -> JoinHandle<KafkaLogResult> {
        // TODO: is rust message pack a good choice? try rkyv instead
        let payload =
            rmp_serde::to_vec(&request).expect("requests should always serialize with rmp");

        self.num_requests.fetch_add(1, atomic::Ordering::Relaxed);

        self.background_log(payload)
    }

    pub fn log_debug_response<R>(&self, response: &R) -> JoinHandle<KafkaLogResult>
    where
        R: serde::Serialize,
    {
        let payload =
            rmp_serde::to_vec(&response).expect("requests should always serialize with rmp");

        self.num_responses.fetch_add(1, atomic::Ordering::Relaxed);

        self.background_log(payload)
    }
}

#[derive(Debug, Default, From, Serialize)]
pub enum RequestOrMethod {
    Request(JsonRpcRequest),
    /// sometimes we don't have a full request. for example, when we are logging a websocket subscription
    Method(Cow<'static, str>, usize),
    #[default]
    None,
}

/// TODO: instead of a bunch of atomics, this should probably use a RwLock
#[derive(Debug, Derivative)]
#[derivative(Default)]
pub struct Web3Request {
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

    pub request: RequestOrMethod,

    /// Instant that the request was received (or at least close to it)
    /// We use Instant and not timestamps to avoid problems with leap seconds and similar issues
    #[derivative(Default(value = "Instant::now()"))]
    pub start_instant: Instant,
    #[derivative(Default(value = "Instant::now() + Duration::from_secs(295)"))]
    pub expire_instant: Instant,
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

    /// ProxyMode::Debug logs requests and responses with Kafka
    /// TODO: maybe this shouldn't be determined by ProxyMode. A request param should probably enable this
    pub kafka_debug_logger: Option<Arc<KafkaDebugLogger>>,

    /// Cancel-safe channel for sending stats to the buffer
    pub stat_sender: Option<mpsc::UnboundedSender<AppStat>>,
}

impl Default for Authorization {
    fn default() -> Self {
        Authorization::internal().unwrap()
    }
}

impl RequestOrMethod {
    pub fn id(&self) -> Box<RawValue> {
        match self {
            Self::Request(x) => x.id.clone(),
            Self::Method(_, _) => Default::default(),
            Self::None => Default::default(),
        }
    }

    pub fn method(&self) -> &str {
        match self {
            Self::Request(x) => x.method.as_str(),
            Self::Method(x, _) => x,
            Self::None => "unknown",
        }
    }

    /// TODO: should this panic on Self::None|Self::Method?
    pub fn params(&self) -> &serde_json::Value {
        match self {
            Self::Request(x) => &x.params,
            Self::Method(..) => &serde_json::Value::Null,
            Self::None => &serde_json::Value::Null,
        }
    }

    pub fn jsonrpc_request(&self) -> Option<&JsonRpcRequest> {
        match self {
            Self::Request(x) => Some(x),
            _ => None,
        }
    }

    pub fn num_bytes(&self) -> usize {
        match self {
            Self::Method(_, num_bytes) => *num_bytes,
            Self::Request(x) => x.num_bytes(),
            Self::None => 0,
        }
    }
}

// TODO: i think a trait is actually the right thing to use here
#[derive(From)]
pub enum ResponseOrBytes<'a> {
    Json(&'a serde_json::Value),
    Response(&'a jsonrpc::SingleResponse),
    Error(&'a Web3ProxyError),
    Bytes(usize),
}

impl<'a> From<u64> for ResponseOrBytes<'a> {
    fn from(value: u64) -> Self {
        Self::Bytes(value as usize)
    }
}

impl ResponseOrBytes<'_> {
    pub fn num_bytes(&self) -> usize {
        match self {
            Self::Json(x) => serde_json::to_string(x)
                .expect("this should always serialize")
                .len(),
            Self::Response(x) => x.num_bytes(),
            Self::Bytes(num_bytes) => *num_bytes,
            Self::Error(x) => {
                let (_, x) = x.as_response_parts();

                x.num_bytes() as usize
            }
        }
    }
}

impl Web3Request {
    #[allow(clippy::too_many_arguments)]
    async fn new_with_options(
        authorization: Arc<Authorization>,
        chain_id: u64,
        head_block: Option<Web3ProxyBlock>,
        kafka_debug_logger: Option<Arc<KafkaDebugLogger>>,
        max_wait: Option<Duration>,
        mut request: RequestOrMethod,
        stat_sender: Option<mpsc::UnboundedSender<AppStat>>,
        usd_per_cu: Decimal,
        app: Option<&Web3ProxyApp>,
    ) -> Arc<Self> {
        let start_instant = Instant::now();

        // TODO: get this default from config, or from user settings
        let expire_instant = start_instant + max_wait.unwrap_or_else(|| Duration::from_secs(295));

        // let request: RequestOrMethod = request.into();

        // we VERY INTENTIONALLY log to kafka BEFORE calculating the cache key
        // this is because calculating the cache_key may modify the params!
        // for example, if the request specifies "latest" as the block number, we replace it with the actual latest block number
        if let Some(ref kafka_debug_logger) = kafka_debug_logger {
            // TODO: channels might be more ergonomic than spawned futures
            // spawned things run in parallel easier but generally need more Arcs
            kafka_debug_logger.log_debug_request(&request);
        }

        // now that kafka has logged the user's original params, we can calculate the cache key
        let cache_mode = match &mut request {
            RequestOrMethod::Request(x) => CacheMode::new(x, head_block.as_ref(), app).await,
            _ => CacheMode::CacheNever,
        };

        let x = Self {
            archive_request: false.into(),
            authorization,
            backend_requests: Default::default(),
            cache_mode,
            chain_id,
            error_response: false.into(),
            expire_instant,
            head_block: head_block.clone(),
            kafka_debug_logger,
            no_servers: 0.into(),
            request,
            response_bytes: 0.into(),
            response_from_backup_rpc: false.into(),
            response_millis: 0.into(),
            response_timestamp: 0.into(),
            start_instant,
            stat_sender,
            usd_per_cu,
            user_error_response: false.into(),
        };

        Arc::new(x)
    }

    pub async fn new_with_app(
        app: &Web3ProxyApp,
        authorization: Arc<Authorization>,
        max_wait: Option<Duration>,
        request: RequestOrMethod,
        head_block: Option<Web3ProxyBlock>,
    ) -> Arc<Self> {
        // TODO: get this out of tracing instead (where we have a String from Amazon's LB)
        let request_ulid = Ulid::new();

        let kafka_debug_logger = if matches!(authorization.checks.proxy_mode, ProxyMode::Debug) {
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

        let stat_sender = app.stat_sender.clone();

        let usd_per_cu = app.config.usd_per_cu.unwrap_or_default();

        Self::new_with_options(
            authorization,
            chain_id,
            head_block,
            kafka_debug_logger,
            max_wait,
            request,
            stat_sender,
            usd_per_cu,
            Some(app),
        )
        .await
    }

    pub async fn new_internal<P: JsonRpcParams>(
        method: String,
        params: &P,
        head_block: Option<Web3ProxyBlock>,
        max_wait: Option<Duration>,
    ) -> Arc<Self> {
        let authorization = Arc::new(Authorization::internal().unwrap());

        // TODO: we need a real id! increment a counter on the app
        let id = JsonRpcId::Number(1);

        // TODO: this seems inefficient
        let request = JsonRpcRequest::new(id, method, json!(params)).unwrap();

        if let Some(app) = APP.get() {
            Self::new_with_app(app, authorization, max_wait, request.into(), head_block).await
        } else {
            Self::new_with_options(
                authorization,
                0,
                head_block,
                None,
                max_wait,
                request.into(),
                None,
                Default::default(),
                None,
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
            CacheMode::CacheNever => None,
            x => {
                let x = JsonRpcQueryCacheKey::new(x, &self.request).hash();

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
        self.request.id()
    }

    pub fn max_block_needed(&self) -> Option<U64> {
        self.cache_mode.to_block().map(|x| *x.num())
    }

    pub fn min_block_needed(&self) -> Option<U64> {
        if self.archive_request.load(atomic::Ordering::Relaxed) {
            Some(U64::zero())
        } else {
            self.cache_mode.from_block().map(|x| *x.num())
        }
    }

    pub fn ttl(&self) -> Duration {
        self.expire_instant
            .saturating_duration_since(Instant::now())
    }

    pub fn ttl_expired(&self) -> bool {
        self.expire_instant < Instant::now()
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

        let num_bytes = response.num_bytes() as u64;

        self.response_bytes
            .fetch_add(num_bytes, atomic::Ordering::Relaxed);

        self.response_millis.fetch_add(
            self.start_instant.elapsed().as_millis() as u64,
            atomic::Ordering::Relaxed,
        );

        // TODO: record first or last timestamp? really, we need multiple
        self.response_timestamp
            .store(Utc::now().timestamp(), atomic::Ordering::Relaxed);

        // TODO: set user_error_response and error_response here instead of outside this function

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

// TODO: is this where the panic comes from?
impl Drop for Web3Request {
    fn drop(&mut self) {
        if self.stat_sender.is_some() {
            // turn `&mut self` into `self`
            let x = mem::take(self);

            trace!(?x, "request metadata dropped without stat send");
            let _ = x.try_send_stat();
        }
    }
}

impl Default for RpcSecretKey {
    fn default() -> Self {
        Self::new()
    }
}

impl Display for RpcSecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // TODO: do this without dereferencing
        let ulid: Ulid = (*self).into();

        Display::fmt(&ulid, f)
    }
}

impl FromStr for RpcSecretKey {
    type Err = Web3ProxyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(ulid) = s.parse::<Ulid>() {
            Ok(ulid.into())
        } else if let Ok(uuid) = s.parse::<Uuid>() {
            Ok(uuid.into())
        } else {
            // TODO: custom error type so that this shows as a 400
            Err(Web3ProxyError::InvalidUserKey)
        }
    }
}

impl From<Ulid> for RpcSecretKey {
    fn from(x: Ulid) -> Self {
        RpcSecretKey::Ulid(x)
    }
}

impl From<Uuid> for RpcSecretKey {
    fn from(x: Uuid) -> Self {
        RpcSecretKey::Uuid(x)
    }
}

impl From<RpcSecretKey> for Ulid {
    fn from(x: RpcSecretKey) -> Self {
        match x {
            RpcSecretKey::Ulid(x) => x,
            RpcSecretKey::Uuid(x) => Ulid::from(x.as_u128()),
        }
    }
}

impl From<RpcSecretKey> for Uuid {
    fn from(x: RpcSecretKey) -> Self {
        match x {
            RpcSecretKey::Ulid(x) => Uuid::from_u128(x.0),
            RpcSecretKey::Uuid(x) => x,
        }
    }
}

impl Authorization {
    /// this acquires a read lock on the latest balance. Be careful not to deadlock!
    pub async fn active_premium(&self) -> bool {
        let user_balance = self.checks.latest_balance.read().await;

        user_balance.active_premium()
    }

    pub fn internal() -> Web3ProxyResult<Self> {
        let authorization_checks = AuthorizationChecks {
            // any error logs on a local (internal) query are likely problems. log them all
            log_revert_chance: 100,
            // default for everything else should be fine. we don't have a user_id or ip to give
            ..Default::default()
        };

        let ip: IpAddr = "127.0.0.1".parse().expect("localhost should always parse");
        let user_agent = UserAgent::from_str(APP_USER_AGENT).ok();

        Self::try_new(
            authorization_checks,
            &ip,
            None,
            None,
            user_agent.as_ref(),
            AuthorizationType::Internal,
        )
    }

    pub fn external(
        allowed_origin_requests_per_period: &HashMap<String, u64>,
        ip: &IpAddr,
        origin: Option<&Origin>,
        proxy_mode: ProxyMode,
        referer: Option<&Referer>,
        user_agent: Option<&UserAgent>,
    ) -> Web3ProxyResult<Self> {
        // some origins can override max_requests_per_period for anon users
        // TODO: i don't like the `to_string` here
        let max_requests_per_period = origin
            .map(|origin| {
                allowed_origin_requests_per_period
                    .get(&origin.to_string())
                    .cloned()
            })
            .unwrap_or_default();

        let authorization_checks = AuthorizationChecks {
            max_requests_per_period,
            proxy_mode,
            ..Default::default()
        };

        Self::try_new(
            authorization_checks,
            ip,
            origin,
            referer,
            user_agent,
            AuthorizationType::Frontend,
        )
    }

    pub fn try_new(
        authorization_checks: AuthorizationChecks,
        ip: &IpAddr,
        origin: Option<&Origin>,
        referer: Option<&Referer>,
        user_agent: Option<&UserAgent>,
        authorization_type: AuthorizationType,
    ) -> Web3ProxyResult<Self> {
        // check ip
        match &authorization_checks.allowed_ips {
            None => {}
            Some(allowed_ips) => {
                if !allowed_ips.iter().any(|x| x.contains(ip)) {
                    return Err(Web3ProxyError::IpNotAllowed(ip.to_owned()));
                }
            }
        }

        // check origin
        match (origin, &authorization_checks.allowed_origins) {
            (None, None) => {}
            (Some(_), None) => {}
            (None, Some(_)) => return Err(Web3ProxyError::OriginRequired),
            (Some(origin), Some(allowed_origins)) => {
                if !allowed_origins.contains(origin) {
                    return Err(Web3ProxyError::OriginNotAllowed(origin.to_owned()));
                }
            }
        }

        // check referer
        match (referer, &authorization_checks.allowed_referers) {
            (None, None) => {}
            (Some(_), None) => {}
            (None, Some(_)) => return Err(Web3ProxyError::RefererRequired),
            (Some(referer), Some(allowed_referers)) => {
                if !allowed_referers.contains(referer) {
                    return Err(Web3ProxyError::RefererNotAllowed(referer.to_owned()));
                }
            }
        }

        // check user_agent
        match (user_agent, &authorization_checks.allowed_user_agents) {
            (None, None) => {}
            (Some(_), None) => {}
            (None, Some(_)) => return Err(Web3ProxyError::UserAgentRequired),
            (Some(user_agent), Some(allowed_user_agents)) => {
                if !allowed_user_agents.contains(user_agent) {
                    return Err(Web3ProxyError::UserAgentNotAllowed(user_agent.to_owned()));
                }
            }
        }

        Ok(Self {
            checks: authorization_checks,
            ip: *ip,
            origin: origin.cloned(),
            referer: referer.cloned(),
            user_agent: user_agent.cloned(),
            authorization_type,
        })
    }
}

/// rate limit logins only by ip.
/// we want all origins and referers and user agents to count together
pub async fn login_is_authorized(app: &Web3ProxyApp, ip: IpAddr) -> Web3ProxyResult<Authorization> {
    let authorization = match app.rate_limit_login(ip, ProxyMode::Best).await? {
        RateLimitResult::Allowed(authorization, None) => authorization,
        RateLimitResult::RateLimited(authorization, retry_at) => {
            return Err(Web3ProxyError::RateLimited(authorization, retry_at));
        }
        // TODO: don't panic. give the user an error
        x => unimplemented!("rate_limit_login shouldn't ever see these: {:?}", x),
    };

    Ok(authorization)
}

/// semaphore won't ever be None, but its easier if key auth and ip auth work the same way
/// keep the semaphore alive until the user's request is entirely complete
pub async fn ip_is_authorized(
    app: &Arc<Web3ProxyApp>,
    ip: &IpAddr,
    origin: Option<&Origin>,
    proxy_mode: ProxyMode,
) -> Web3ProxyResult<(Authorization, Option<OwnedSemaphorePermit>)> {
    // TODO: i think we could write an `impl From` for this
    // TODO: move this to an AuthorizedUser extrator
    let (authorization, semaphore) = match app.rate_limit_public(ip, origin, proxy_mode).await? {
        RateLimitResult::Allowed(authorization, semaphore) => (authorization, semaphore),
        RateLimitResult::RateLimited(authorization, retry_at) => {
            // TODO: in the background, emit a stat (maybe simplest to use a channel?)
            return Err(Web3ProxyError::RateLimited(authorization, retry_at));
        }
        // TODO: don't panic. give the user an error
        x => unimplemented!("rate_limit_by_ip shouldn't ever see these: {:?}", x),
    };

    // in the background, add the hashed ip to a recent_users map
    if app.config.public_recent_ips_salt.is_some() {
        let app = app.clone();
        let ip = *ip;

        let f = async move {
            let now = Utc::now().timestamp();

            if let Ok(mut redis_conn) = app.redis_conn().await {
                let salt = app
                    .config
                    .public_recent_ips_salt
                    .as_ref()
                    .expect("public_recent_ips_salt must exist in here");

                let salted_ip = format!("{}:{}", salt, ip);

                let hashed_ip = Bytes::from(keccak256(salted_ip.as_bytes()));

                let recent_ip_key = format!("recent_users:ip:{}", app.config.chain_id);

                redis_conn
                    .zadd(recent_ip_key, hashed_ip.to_string(), now)
                    .await?;
            };

            Ok::<_, Web3ProxyError>(())
        }
        .map_err(|err| {
            warn!(?err, "background update of recent_users:ip failed");

            err
        });

        tokio::spawn(f);
    }

    Ok((authorization, semaphore))
}

/// like app.rate_limit_by_rpc_key but converts to a Web3ProxyError;
/// keep the semaphore alive until the user's request is entirely complete
pub async fn key_is_authorized(
    app: &Arc<Web3ProxyApp>,
    rpc_key: &RpcSecretKey,
    ip: &IpAddr,
    origin: Option<&Origin>,
    proxy_mode: ProxyMode,
    referer: Option<&Referer>,
    user_agent: Option<&UserAgent>,
) -> Web3ProxyResult<(Authorization, Option<OwnedSemaphorePermit>)> {
    // check the rate limits. error if over the limit
    // TODO: i think this should be in an "impl From" or "impl Into"
    let (authorization, semaphore) = match app
        .rate_limit_premium(ip, origin, proxy_mode, referer, rpc_key, user_agent)
        .await?
    {
        RateLimitResult::Allowed(authorization, semaphore) => (authorization, semaphore),
        RateLimitResult::RateLimited(authorization, retry_at) => {
            return Err(Web3ProxyError::RateLimited(authorization, retry_at));
        }
        RateLimitResult::UnknownKey => return Err(Web3ProxyError::UnknownKey),
    };

    // TODO: DRY and maybe optimize the hashing
    // in the background, add the ip to a recent_users map
    if app.config.public_recent_ips_salt.is_some() {
        let app = app.clone();
        let user_id = authorization.checks.user_id;
        let f = async move {
            let now = Utc::now().timestamp();

            if let Ok(mut redis_conn) = app.redis_conn().await {
                let salt = app
                    .config
                    .public_recent_ips_salt
                    .as_ref()
                    .expect("public_recent_ips_salt must exist in here");

                let salted_user_id = format!("{}:{}", salt, user_id);

                let hashed_user_id = Bytes::from(keccak256(salted_user_id.as_bytes()));

                let recent_user_id_key = format!("recent_users:id:{}", app.config.chain_id);

                redis_conn
                    .zadd(recent_user_id_key, hashed_user_id.to_string(), now)
                    .await?;
            }

            Ok::<_, Web3ProxyError>(())
        }
        .map_err(|err| {
            warn!(?err, "background update of recent_users:id failed");

            err
        });

        tokio::spawn(f);
    }

    Ok((authorization, semaphore))
}

impl Web3ProxyApp {
    /// Limit the number of concurrent requests from the given ip address.
    pub async fn permit_public_concurrency(
        &self,
        ip: &IpAddr,
    ) -> Web3ProxyResult<Option<OwnedSemaphorePermit>> {
        if let Some(max_concurrent_requests) = self.config.public_max_concurrent_requests {
            let semaphore = self
                .ip_semaphores
                .get_with_by_ref(ip, async {
                    // TODO: set max_concurrent_requests dynamically based on load?
                    let s = Semaphore::new(max_concurrent_requests);
                    Arc::new(s)
                })
                .await;

            let semaphore_permit = tokio::select! {
                biased;

                p = semaphore.acquire_owned() => {
                    p
                }
                p = self.bonus_ip_concurrency.clone().acquire_owned() => {
                    p
                }
            }?;

            Ok(Some(semaphore_permit))
        } else {
            Ok(None)
        }
    }

    /// Limit the number of concurrent requests for a given user across all of their keys
    /// keep the semaphore alive until the user's request is entirely complete
    pub async fn permit_premium_concurrency(
        &self,
        authorization: &Authorization,
        ip: &IpAddr,
    ) -> Web3ProxyResult<Option<OwnedSemaphorePermit>> {
        let authorization_checks = &authorization.checks;

        if let Some(max_concurrent_requests) = authorization_checks.max_concurrent_requests {
            let user_id = authorization_checks
                .user_id
                .try_into()
                .or(Err(Web3ProxyError::UserIdZero))?;

            let semaphore = self
                .user_semaphores
                .get_with_by_ref(&(user_id, *ip), async move {
                    let s = Semaphore::new(max_concurrent_requests as usize);
                    Arc::new(s)
                })
                .await;

            let semaphore_permit = tokio::select! {
                biased;

                p = semaphore.acquire_owned() => {
                    p
                }
                p = self.bonus_user_concurrency.clone().acquire_owned() => {
                    p
                }
                p = self.bonus_ip_concurrency.clone().acquire_owned() => {
                    p
                }
            }?;

            Ok(Some(semaphore_permit))
        } else {
            // unlimited concurrency
            Ok(None)
        }
    }

    /// Verify that the given bearer token and address are allowed to take the specified action.
    /// This includes concurrent request limiting.
    /// keep the semaphore alive until the user's request is entirely complete
    pub async fn bearer_is_authorized(&self, bearer: Bearer) -> Web3ProxyResult<user::Model> {
        // get the user id for this bearer token
        let user_bearer_token = UserBearerToken::try_from(bearer)?;

        // get the attached address from the database for the given auth_token.
        let db_replica = global_db_replica_conn()?;

        let user_bearer_uuid: Uuid = user_bearer_token.into();

        let user = user::Entity::find()
            .left_join(login::Entity)
            .filter(login::Column::BearerToken.eq(user_bearer_uuid))
            .one(db_replica.as_ref())
            .await
            .web3_context("fetching user from db by bearer token")?
            .web3_context("unknown bearer token")?;

        Ok(user)
    }

    pub async fn rate_limit_login(
        &self,
        ip: IpAddr,
        proxy_mode: ProxyMode,
    ) -> Web3ProxyResult<RateLimitResult> {
        // TODO: if ip is on the local network, always allow?

        // we don't care about user agent or origin or referer
        let authorization = Authorization::external(
            &self.config.allowed_origin_requests_per_period,
            &ip,
            None,
            proxy_mode,
            None,
            None,
        )?;

        let label = ip.to_string();

        redis_rate_limit(
            &self.login_rate_limiter,
            authorization,
            None,
            Some(&label),
            None,
        )
        .await
    }

    /// origin is included because it can override the default rate limits
    pub async fn rate_limit_public(
        &self,
        ip: &IpAddr,
        origin: Option<&Origin>,
        proxy_mode: ProxyMode,
    ) -> Web3ProxyResult<RateLimitResult> {
        if ip.is_loopback() {
            // TODO: localhost being unlimited should be optional
            let authorization = Authorization::internal()?;

            return Ok(RateLimitResult::Allowed(authorization, None));
        }

        let allowed_origin_requests_per_period = &self.config.allowed_origin_requests_per_period;

        // ip rate limits don't check referer or user agent
        // they do check origin because we can override rate limits for some origins
        let authorization = Authorization::external(
            allowed_origin_requests_per_period,
            ip,
            origin,
            proxy_mode,
            None,
            None,
        )?;

        if let Some(rate_limiter) = &self.frontend_public_rate_limiter {
            let mut x = deferred_redis_rate_limit(authorization, *ip, None, rate_limiter).await?;

            if let RateLimitResult::RateLimited(authorization, retry_at) = x {
                // we got rate limited, try bonus_frontend_public_rate_limiter
                x = redis_rate_limit(
                    &self.bonus_frontend_public_rate_limiter,
                    authorization,
                    retry_at,
                    None,
                    None,
                )
                .await?;
            }

            if let RateLimitResult::Allowed(a, b) = x {
                debug_assert!(b.is_none());

                let permit = self.permit_public_concurrency(ip).await?;

                x = RateLimitResult::Allowed(a, permit)
            }

            debug_assert!(!matches!(x, RateLimitResult::UnknownKey));

            Ok(x)
        } else {
            // no redis, but we can still check the ip semaphore
            let permit = self.permit_public_concurrency(ip).await?;

            // TODO: if no redis, rate limit with a local cache? "warn!" probably isn't right
            Ok(RateLimitResult::Allowed(authorization, permit))
        }
    }

    // check the local cache for user data, or query the database
    pub(crate) async fn authorization_checks(
        &self,
        proxy_mode: ProxyMode,
        rpc_secret_key: &RpcSecretKey,
    ) -> Web3ProxyResult<AuthorizationChecks> {
        // TODO: move onto a helper function

        let x = self
            .rpc_secret_key_cache
            .try_get_with_by_ref(rpc_secret_key, async move {
                let db_replica = global_db_replica_conn()?;

                // TODO: join the user table to this to return the User? we don't always need it
                // TODO: join on secondary users
                // TODO: join on user tier
                match rpc_key::Entity::find()
                    .filter(rpc_key::Column::SecretKey.eq(<Uuid>::from(*rpc_secret_key)))
                    .filter(rpc_key::Column::Active.eq(true))
                    .one(db_replica.as_ref())
                    .await?
                {
                    Some(rpc_key_model) => {
                        // TODO: move these splits into helper functions
                        // TODO: can we have sea orm handle this for us?
                        let allowed_ips: Option<Vec<IpNet>> =
                            if let Some(allowed_ips) = rpc_key_model.allowed_ips {
                                let x = allowed_ips
                                    .split(',')
                                    .map(|x| x.trim().parse::<IpNet>())
                                    .collect::<Result<Vec<_>, _>>()?;
                                Some(x)
                            } else {
                                None
                            };

                        let allowed_origins: Option<Vec<Origin>> =
                            if let Some(allowed_origins) = rpc_key_model.allowed_origins {
                                // TODO: do this without collecting twice?
                                let x = allowed_origins
                                    .split(',')
                                    .map(|x| HeaderValue::from_str(x.trim()))
                                    .collect::<Result<Vec<_>, _>>()?
                                    .into_iter()
                                    .map(|x| Origin::decode(&mut [x].iter()))
                                    .collect::<Result<Vec<_>, _>>()?;

                                Some(x)
                            } else {
                                None
                            };

                        let allowed_referers: Option<Vec<Referer>> =
                            if let Some(allowed_referers) = rpc_key_model.allowed_referers {
                                let x = allowed_referers
                                    .split(',')
                                    .map(|x| {
                                        x.trim()
                                            .parse::<Referer>()
                                            .or(Err(Web3ProxyError::InvalidReferer))
                                    })
                                    .collect::<Result<Vec<_>, _>>()?;

                                Some(x)
                            } else {
                                None
                            };

                        let allowed_user_agents: Option<Vec<UserAgent>> =
                            if let Some(allowed_user_agents) = rpc_key_model.allowed_user_agents {
                                let x: Result<Vec<_>, _> = allowed_user_agents
                                    .split(',')
                                    .map(|x| {
                                        x.trim()
                                            .parse::<UserAgent>()
                                            .or(Err(Web3ProxyError::InvalidUserAgent))
                                    })
                                    .collect();

                                Some(x?)
                            } else {
                                None
                            };

                        // Get the user_tier
                        let user_model = user::Entity::find_by_id(rpc_key_model.user_id)
                            .one(db_replica.as_ref())
                            .await?
                            .web3_context(
                                "user model was not found, but every rpc_key should have a user",
                            )?;

                        let mut user_tier_model = user_tier::Entity::find_by_id(
                            user_model.user_tier_id,
                        )
                        .one(db_replica.as_ref())
                        .await?
                        .web3_context(
                            "related user tier not found, but every user should have a tier",
                        )?;

                        let latest_balance = self
                            .user_balance_cache
                            .get_or_insert(db_replica.as_ref(), rpc_key_model.user_id)
                            .await?;

                        let paid_credits_used: bool;
                        if let Some(downgrade_user_tier) = user_tier_model.downgrade_tier_id {
                            trace!("user belongs to a premium tier. checking balance");

                            let active_premium = latest_balance.read().await.active_premium();

                            // only consider the user premium if they have paid at least $10 and have a balance > $.01
                            // otherwise, set user_tier_model to the downograded tier
                            if active_premium {
                                paid_credits_used = true;
                            } else {
                                paid_credits_used = false;

                                // TODO: include boolean to mark that the user is downgraded
                                user_tier_model =
                                    user_tier::Entity::find_by_id(downgrade_user_tier)
                                        .one(db_replica.as_ref())
                                        .await?
                                        .web3_context(format!(
                                            "downgrade user tier ({}) is missing!",
                                            downgrade_user_tier
                                        ))?;
                            }
                        } else {
                            paid_credits_used = false;
                        }

                        let rpc_key_id =
                            Some(rpc_key_model.id.try_into().context("db ids are never 0")?);

                        Ok::<_, Web3ProxyError>(AuthorizationChecks {
                            allowed_ips,
                            allowed_origins,
                            allowed_referers,
                            allowed_user_agents,
                            latest_balance,
                            // TODO: is floating point math going to scale this correctly?
                            log_revert_chance: (rpc_key_model.log_revert_chance * u16::MAX as f64)
                                as u16,
                            max_concurrent_requests: user_tier_model.max_concurrent_requests,
                            max_requests_per_period: user_tier_model.max_requests_per_period,
                            private_txs: rpc_key_model.private_txs,
                            proxy_mode,
                            rpc_secret_key: Some(*rpc_secret_key),
                            rpc_secret_key_id: rpc_key_id,
                            user_id: rpc_key_model.user_id,
                            paid_credits_used,
                        })
                    }
                    None => Ok(AuthorizationChecks::default()),
                }
            })
            .await?;

        Ok(x)
    }

    /// Authorize the key/ip/origin/referer/useragent and handle rate and concurrency limits
    pub async fn rate_limit_premium(
        &self,
        ip: &IpAddr,
        origin: Option<&Origin>,
        proxy_mode: ProxyMode,
        referer: Option<&Referer>,
        rpc_key: &RpcSecretKey,
        user_agent: Option<&UserAgent>,
    ) -> Web3ProxyResult<RateLimitResult> {
        let authorization_checks = match self.authorization_checks(proxy_mode, rpc_key).await {
            Ok(x) => x,
            Err(err) => {
                if let Ok(_err) = err.split_db_errors() {
                    // // TODO: this is too verbose during an outage. the warnings on the config reloader should be fine
                    // warn!(
                    //     ?err,
                    //     "db is down. cannot check rpc key. fallback to ip rate limits"
                    // );
                    return self.rate_limit_public(ip, origin, proxy_mode).await;
                }

                return Err(err);
            }
        };

        // if no rpc_key_id matching the given rpc was found, then we can't rate limit by key
        if authorization_checks.rpc_secret_key_id.is_none() {
            trace!("unknown key. falling back to free limits");
            return self.rate_limit_public(ip, origin, proxy_mode).await;
        }

        let authorization = Authorization::try_new(
            authorization_checks,
            ip,
            origin,
            referer,
            user_agent,
            AuthorizationType::Frontend,
        )?;

        // user key is valid. now check rate limits
        if let Some(user_max_requests_per_period) = authorization.checks.max_requests_per_period {
            if let Some(rate_limiter) = &self.frontend_premium_rate_limiter {
                let key = RegisteredUserRateLimitKey(authorization.checks.user_id, *ip);

                let mut x = deferred_redis_rate_limit(
                    authorization,
                    key,
                    Some(user_max_requests_per_period),
                    rate_limiter,
                )
                .await?;

                if let RateLimitResult::RateLimited(authorization, retry_at) = x {
                    // rate limited by the user's key+ip. check to see if there are any limits available in the bonus premium pool
                    x = redis_rate_limit(
                        &self.bonus_frontend_premium_rate_limiter,
                        authorization,
                        retry_at,
                        None,
                        None,
                    )
                    .await?;
                }

                if let RateLimitResult::RateLimited(authorization, retry_at) = x {
                    // premium got rate limited too. check the bonus public pool
                    x = redis_rate_limit(
                        &self.bonus_frontend_public_rate_limiter,
                        authorization,
                        retry_at,
                        None,
                        None,
                    )
                    .await?;
                }

                if let RateLimitResult::Allowed(a, b) = x {
                    debug_assert!(b.is_none());

                    // only allow this rpc_key to run a limited amount of concurrent requests
                    let permit = self.permit_premium_concurrency(&a, ip).await?;

                    x = RateLimitResult::Allowed(a, permit)
                }

                debug_assert!(!matches!(x, RateLimitResult::UnknownKey));

                return Ok(x);
            } else {
                // TODO: if no redis, rate limit with just a local cache?
            }
        }

        let permit = self.permit_premium_concurrency(&authorization, ip).await?;

        Ok(RateLimitResult::Allowed(authorization, permit))
    }
}

impl Authorization {
    pub async fn check_again(
        &self,
        app: &Arc<Web3ProxyApp>,
    ) -> Web3ProxyResult<(Arc<Self>, Option<OwnedSemaphorePermit>)> {
        // TODO: we could probably do this without clones. but this is easy
        let (a, s) = if let Some(ref rpc_secret_key) = self.checks.rpc_secret_key {
            key_is_authorized(
                app,
                rpc_secret_key,
                &self.ip,
                self.origin.as_ref(),
                self.checks.proxy_mode,
                self.referer.as_ref(),
                self.user_agent.as_ref(),
            )
            .await?
        } else {
            ip_is_authorized(app, &self.ip, self.origin.as_ref(), self.checks.proxy_mode).await?
        };

        let a = Arc::new(a);

        Ok((a, s))
    }
}

/// this fails open!
/// this never includes a semaphore! if you want one, add it after this call
/// if `max_requests_per_period` is none, the limit in the authorization is used
pub async fn deferred_redis_rate_limit<K>(
    authorization: Authorization,
    key: K,
    max_requests_per_period: Option<u64>,
    rate_limiter: &DeferredRateLimiter<K>,
) -> Web3ProxyResult<RateLimitResult>
where
    K: Send + Sync + Copy + Clone + Display + Hash + Eq + PartialEq + 'static,
{
    let max_requests_per_period =
        max_requests_per_period.or(authorization.checks.max_requests_per_period);

    let x = match rate_limiter.throttle(key, max_requests_per_period, 1).await {
        Ok(DeferredRateLimitResult::Allowed) => RateLimitResult::Allowed(authorization, None),
        Ok(DeferredRateLimitResult::RetryAt(retry_at)) => {
            // TODO: set headers so they know when they can retry
            // TODO: debug or trace?
            // this is too verbose, but a stat might be good
            // TODO: emit a stat
            // trace!(?rpc_key, "rate limit exceeded until {:?}", retry_at);
            RateLimitResult::RateLimited(authorization, Some(retry_at))
        }
        Ok(DeferredRateLimitResult::RetryNever) => {
            // TODO: keys are secret. don't log them!
            // trace!(?rpc_key, "rate limit is 0");
            // TODO: emit a stat
            RateLimitResult::RateLimited(authorization, None)
        }
        Err(err) => {
            // internal error, not rate limit being hit
            error!(?err, %key, "rate limiter is unhappy. allowing key");

            RateLimitResult::Allowed(authorization, None)
        }
    };

    Ok(x)
}

/// this never includes a semaphore! if you want one, add it after this call
/// if `max_requests_per_period` is none, the limit in the authorization is used
pub async fn redis_rate_limit(
    rate_limiter: &Option<RedisRateLimiter>,
    authorization: Authorization,
    mut retry_at: Option<Instant>,
    label: Option<&str>,
    max_requests_per_period: Option<u64>,
) -> Web3ProxyResult<RateLimitResult> {
    let max_requests_per_period =
        max_requests_per_period.or(authorization.checks.max_requests_per_period);

    let x = if let Some(rate_limiter) = rate_limiter {
        match rate_limiter
            .throttle_label(label.unwrap_or_default(), max_requests_per_period, 1)
            .await
        {
            Ok(RedisRateLimitResult::Allowed(..)) => RateLimitResult::Allowed(authorization, None),
            Ok(RedisRateLimitResult::RetryAt(new_retry_at, ..)) => {
                retry_at = retry_at.min(Some(new_retry_at));

                RateLimitResult::RateLimited(authorization, retry_at)
            }
            Ok(RedisRateLimitResult::RetryNever) => {
                RateLimitResult::RateLimited(authorization, retry_at)
            }
            Err(err) => {
                // this an internal error of some kind, not the rate limit being hit
                error!("rate limiter is unhappy. allowing ip. err={:?}", err);

                RateLimitResult::Allowed(authorization, None)
            }
        }
    } else {
        RateLimitResult::Allowed(authorization, None)
    };

    Ok(x)
}
