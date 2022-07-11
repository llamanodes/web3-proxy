///! Rate-limited communication with a web3 provider
use anyhow::Context;
use derive_more::From;
use ethers::prelude::{Block, Bytes, Middleware, ProviderError, TxHash, U256};
use futures::future::try_join_all;
use futures::StreamExt;
use redis_cell_client::RedisCellClient;
use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{self, AtomicBool, AtomicU32};
use std::{cmp::Ordering, sync::Arc};
use tokio::sync::broadcast;
use tokio::sync::RwLock;
use tokio::time::{interval, sleep, Duration, MissedTickBehavior};
use tracing::{debug, error, info, instrument, trace, warn};

use crate::app::{flatten_handle, AnyhowJoinHandle};

/// TODO: instead of an enum, I tried to use Box<dyn Provider>, but hit https://github.com/gakonst/ethers-rs/issues/592
#[derive(From)]
pub enum Web3Provider {
    Http(ethers::providers::Provider<ethers::providers::Http>),
    Ws(ethers::providers::Provider<ethers::providers::Ws>),
}

impl Web3Provider {
    #[instrument]
    async fn from_str(
        url_str: &str,
        http_client: Option<&reqwest::Client>,
    ) -> anyhow::Result<Self> {
        let provider = if url_str.starts_with("http") {
            let url: url::Url = url_str.parse()?;

            let http_client = http_client
                .ok_or_else(|| anyhow::anyhow!("no http_client"))?
                .clone();

            let provider = ethers::providers::Http::new_with_client(url, http_client);

            // TODO: dry this up (needs https://github.com/gakonst/ethers-rs/issues/592)
            // TODO: i don't think this interval matters for our uses, but we should probably set it to like `block time / 2`
            ethers::providers::Provider::new(provider)
                .interval(Duration::from_secs(13))
                .into()
        } else if url_str.starts_with("ws") {
            // TODO: wrapper automatically reconnect
            let provider = ethers::providers::Ws::connect(url_str).await?;

            // TODO: dry this up (needs https://github.com/gakonst/ethers-rs/issues/592)
            // TODO: i don't think this interval matters
            ethers::providers::Provider::new(provider)
                .interval(Duration::from_secs(1))
                .into()
        } else {
            return Err(anyhow::anyhow!("only http and ws servers are supported"));
        };

        Ok(provider)
    }
}

impl fmt::Debug for Web3Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default Debug takes forever to write. this is too quiet though. we at least need the url
        f.debug_struct("Web3Provider").finish_non_exhaustive()
    }
}

/// An active connection to a Web3Rpc
pub struct Web3Connection {
    /// TODO: can we get this from the provider? do we even need it?
    url: String,
    /// keep track of currently open requests. We sort on this
    active_requests: AtomicU32,
    /// provider is in a RwLock so that we can replace it if re-connecting
    provider: RwLock<Option<Arc<Web3Provider>>>,
    /// rate limits are stored in a central redis so that multiple proxies can share their rate limits
    hard_limit: Option<redis_cell_client::RedisCellClient>,
    /// used for load balancing to the least loaded server
    soft_limit: u32,
    archive: AtomicBool,
}

impl Serialize for Web3Connection {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // 3 is the number of fields in the struct.
        let mut state = serializer.serialize_struct("Web3Connection", 3)?;

        // TODO: sanitize any credentials in the url
        state.serialize_field("url", &self.url)?;

        state.serialize_field("soft_limit", &self.soft_limit)?;

        state.serialize_field(
            "active_requests",
            &self.active_requests.load(atomic::Ordering::Relaxed),
        )?;

        state.end()
    }
}
impl fmt::Debug for Web3Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Web3Connection")
            .field("url", &self.url)
            .field("archive", &self.is_archive())
            .finish_non_exhaustive()
    }
}

impl fmt::Display for Web3Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", &self.url)
    }
}

impl Web3Connection {
    /// Connect to a web3 rpc
    // #[instrument(name = "spawn_Web3Connection", skip(hard_limit, http_client))]
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn(
        chain_id: usize,
        url_str: String,
        // optional because this is only used for http providers. websocket providers don't use it
        http_client: Option<&reqwest::Client>,
        http_interval_sender: Option<Arc<broadcast::Sender<()>>>,
        hard_limit: Option<(u32, &redis_cell_client::RedisClientPool)>,
        // TODO: think more about this type
        soft_limit: u32,
        block_sender: Option<flume::Sender<(Block<TxHash>, Arc<Self>)>>,
        tx_id_sender: Option<flume::Sender<(TxHash, Arc<Self>)>>,
        reconnect: bool,
    ) -> anyhow::Result<(Arc<Web3Connection>, AnyhowJoinHandle<()>)> {
        let hard_limit = hard_limit.map(|(hard_rate_limit, redis_conection)| {
            // TODO: allow different max_burst and count_per_period and period
            let period = 1;
            RedisCellClient::new(
                redis_conection.clone(),
                format!("{},{}", chain_id, url_str),
                hard_rate_limit,
                hard_rate_limit,
                period,
            )
        });

        let provider = Web3Provider::from_str(&url_str, http_client).await?;

        let new_connection = Self {
            url: url_str.clone(),
            active_requests: 0.into(),
            provider: RwLock::new(Some(Arc::new(provider))),
            hard_limit,
            soft_limit,
            archive: Default::default(),
        };

        let new_connection = Arc::new(new_connection);

        // check the server's chain_id here
        // TODO: move this outside the `new` function and into a `start` function or something
        // TODO: some public rpcs (on bsc and fantom) do not return an id and so this ends up being an error
        // TODO: this will wait forever. do we want that?
        let found_chain_id: Result<String, _> = new_connection
            .wait_for_request_handle()
            .await
            .request("eth_chainId", Option::None::<()>)
            .await;

        match found_chain_id {
            Ok(found_chain_id) => {
                // TODO: there has to be a cleaner way to do this
                let found_chain_id =
                    usize::from_str_radix(found_chain_id.trim_start_matches("0x"), 16).unwrap();

                if chain_id != found_chain_id {
                    return Err(anyhow::anyhow!(
                        "incorrect chain id! Expected {}. Found {}",
                        chain_id,
                        found_chain_id
                    ));
                }
            }
            Err(e) => {
                let e = anyhow::Error::from(e).context(format!("{}", new_connection));
                return Err(e);
            }
        }

        // we could take "archive" as a parameter, but we would want a safety check on it regardless
        // just query something very old and if we get an error, we don't have archive data
        let archive_result: Result<Bytes, _> = new_connection
            .wait_for_request_handle()
            .await
            .request(
                "eth_getCode",
                ("0xdead00000000000000000000000000000000beef", "0x1"),
            )
            .await;

        trace!(?archive_result, "{}", new_connection);

        if archive_result.is_ok() {
            new_connection
                .archive
                .store(true, atomic::Ordering::Relaxed);
        }

        info!(?new_connection, "success");

        let handle = {
            let new_connection = new_connection.clone();
            tokio::spawn(async move {
                new_connection
                    .subscribe(http_interval_sender, block_sender, tx_id_sender, reconnect)
                    .await
            })
        };

        Ok((new_connection, handle))
    }

    pub fn is_archive(&self) -> bool {
        self.archive.load(atomic::Ordering::Relaxed)
    }

    #[instrument(skip_all)]
    pub async fn reconnect(
        self: &Arc<Self>,
        block_sender: Option<flume::Sender<(Block<TxHash>, Arc<Self>)>>,
    ) -> anyhow::Result<()> {
        // websocket doesn't need the http client
        let http_client = None;

        // since this lock is held open over an await, we use tokio's locking
        let mut provider = self.provider.write().await;

        *provider = None;

        // tell the block subscriber that we are at 0
        if let Some(block_sender) = block_sender {
            block_sender
                .send_async((Block::default(), self.clone()))
                .await
                .context("block_sender at 0")?;
        }

        // TODO: if this fails, keep retrying
        let new_provider = Web3Provider::from_str(&self.url, http_client).await?;

        *provider = Some(Arc::new(new_provider));

        Ok(())
    }

    #[inline]
    pub fn active_requests(&self) -> u32 {
        self.active_requests.load(atomic::Ordering::Acquire)
    }

    #[inline]
    pub fn soft_limit(&self) -> u32 {
        self.soft_limit
    }

    #[inline]
    pub async fn has_provider(&self) -> bool {
        self.provider.read().await.is_some()
    }

    #[instrument(skip_all)]
    async fn send_block_result(
        self: &Arc<Self>,
        block: Result<Block<TxHash>, ProviderError>,
        block_sender: &flume::Sender<(Block<TxHash>, Arc<Self>)>,
    ) -> anyhow::Result<()> {
        match block {
            Ok(block) => {
                // TODO: i'm pretty sure we don't need send_async, but double check
                block_sender
                    .send_async((block, self.clone()))
                    .await
                    .context("block_sender")?;
            }
            Err(e) => {
                warn!("unable to get block from {}: {}", self, e);
            }
        }

        Ok(())
    }

    async fn subscribe(
        self: Arc<Self>,
        http_interval_sender: Option<Arc<broadcast::Sender<()>>>,
        block_sender: Option<flume::Sender<(Block<TxHash>, Arc<Self>)>>,
        tx_id_sender: Option<flume::Sender<(TxHash, Arc<Self>)>>,
        reconnect: bool,
    ) -> anyhow::Result<()> {
        loop {
            let http_interval_receiver = http_interval_sender.clone().map(|x| x.subscribe());

            let mut futures = vec![];

            if let Some(block_sender) = &block_sender {
                let f = self
                    .clone()
                    .subscribe_new_heads(http_interval_receiver, block_sender.clone());

                futures.push(flatten_handle(tokio::spawn(f)));
            }

            if let Some(tx_id_sender) = &tx_id_sender {
                let f = self
                    .clone()
                    .subscribe_pending_transactions(tx_id_sender.clone());

                futures.push(flatten_handle(tokio::spawn(f)));
            }

            if futures.is_empty() {
                // TODO: is there a better way to make a channel that is never ready?
                info!(?self, "no-op subscription");
                return Ok(());
            }

            match try_join_all(futures).await {
                Ok(_) => break,
                Err(err) => {
                    if reconnect {
                        // TODO: exponential backoff
                        let retry_in = Duration::from_secs(1);
                        warn!(
                            ?self,
                            "subscription exited. Attempting to reconnect in {:?}. {:?}",
                            retry_in,
                            err
                        );
                        sleep(retry_in).await;

                        // TODO: loop on reconnecting! do not return with a "?" here
                        // TODO: this isn't going to work. it will get in a loop with newHeads
                        self.reconnect(block_sender.clone()).await?;
                    } else {
                        error!(?self, ?err, "subscription exited");
                        return Err(err);
                    }
                }
            }
        }

        Ok(())
    }

    /// Subscribe to new blocks. If `reconnect` is true, this runs forever.
    /// TODO: instrument with the url
    #[instrument(skip_all)]
    async fn subscribe_new_heads(
        self: Arc<Self>,
        http_interval_receiver: Option<broadcast::Receiver<()>>,
        block_sender: flume::Sender<(Block<TxHash>, Arc<Self>)>,
    ) -> anyhow::Result<()> {
        info!("watching {}", self);

        // TODO: is a RwLock of an Option<Arc> the right thing here?
        if let Some(provider) = self.provider.read().await.clone() {
            match &*provider {
                Web3Provider::Http(_provider) => {
                    // there is a "watch_blocks" function, but a lot of public nodes do not support the necessary rpc endpoints
                    // TODO: try watch_blocks and fall back to this?

                    let mut http_interval_receiver = http_interval_receiver.unwrap();

                    let mut last_hash = Default::default();

                    loop {
                        match self.try_request_handle().await {
                            Ok(active_request_handle) => {
                                // TODO: i feel like this should be easier. there is a provider.getBlock, but i don't know how to give it "latest"
                                let block: Result<Block<TxHash>, _> = active_request_handle
                                    .request("eth_getBlockByNumber", ("latest", false))
                                    .await;

                                // don't send repeat blocks
                                if let Ok(block) = &block {
                                    let new_hash = block.hash.unwrap();

                                    if new_hash == last_hash {
                                        continue;
                                    }

                                    last_hash = new_hash;
                                }

                                self.send_block_result(block, &block_sender).await?;
                            }
                            Err(err) => {
                                warn!(?err, "Rate limited on latest block from {}", self);
                            }
                        }

                        // wait for the interval
                        // TODO: if error or rate limit, increase interval?
                        while let Err(err) = http_interval_receiver.recv().await {
                            // querying the block was delayed. this can happen if tokio was busy.
                            warn!(?err, ?self, "http interval lagging!")
                        }
                    }
                }
                Web3Provider::Ws(provider) => {
                    let active_request_handle = self.wait_for_request_handle().await;
                    let mut stream = provider.subscribe_blocks().await?;
                    drop(active_request_handle);

                    // query the block once since the subscription doesn't send the current block
                    // there is a very small race condition here where the stream could send us a new block right now
                    // all it does is print "new block" for the same block as current block
                    let block: Result<Block<TxHash>, _> = self
                        .wait_for_request_handle()
                        .await
                        .request("eth_getBlockByNumber", ("latest", false))
                        .await;

                    self.send_block_result(block, &block_sender).await?;

                    while let Some(new_block) = stream.next().await {
                        self.send_block_result(Ok(new_block), &block_sender).await?;
                    }

                    warn!(?self, "subscription ended");
                }
            }
        }

        Ok(())
    }

    #[instrument(skip_all)]
    async fn subscribe_pending_transactions(
        self: Arc<Self>,
        tx_id_sender: flume::Sender<(TxHash, Arc<Self>)>,
    ) -> anyhow::Result<()> {
        info!("watching {}", self);

        // TODO: is a RwLock of an Option<Arc> the right thing here?
        if let Some(provider) = self.provider.read().await.clone() {
            match &*provider {
                Web3Provider::Http(provider) => {
                    // there is a "watch_pending_transactions" function, but a lot of public nodes do not support the necessary rpc endpoints
                    // TODO: what should this interval be? probably automatically set to some fraction of block time
                    // TODO: maybe it would be better to have one interval for all of the http providers, but this works for now
                    // TODO: if there are some websocket providers, maybe have a longer interval and a channel that tells the https to update when a websocket gets a new head? if they are slow this wouldn't work well though
                    let mut interval = interval(Duration::from_secs(60));
                    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

                    loop {
                        // TODO: actually do something here
                        /*
                        match self.try_request_handle().await {
                            Ok(active_request_handle) => {
                                // TODO: check the filter
                                unimplemented!("actually send a request");
                            }
                            Err(e) => {
                                warn!("Failed getting latest block from {}: {:?}", self, e);
                            }
                        }
                        */

                        // wait for the interval
                        // TODO: if error or rate limit, increase interval?
                        interval.tick().await;
                    }
                }
                Web3Provider::Ws(provider) => {
                    // TODO: maybe the subscribe_pending_txs function should be on the active_request_handle
                    let active_request_handle = self.wait_for_request_handle().await;

                    let mut stream = provider.subscribe_pending_txs().await?;

                    drop(active_request_handle);

                    while let Some(pending_tx_id) = stream.next().await {
                        tx_id_sender
                            .send_async((pending_tx_id, self.clone()))
                            .await
                            .context("tx_id_sender")?;
                    }

                    warn!("subscription ended");
                }
            }
        }

        Ok(())
    }

    /// be careful with this; it will wait forever!
    #[instrument(skip_all)]
    pub async fn wait_for_request_handle(self: &Arc<Self>) -> ActiveRequestHandle {
        // TODO: maximum wait time? i think timeouts in other parts of the code are probably best

        loop {
            match self.try_request_handle().await {
                Ok(pending_request_handle) => return pending_request_handle,
                Err(retry_after) => {
                    sleep(retry_after).await;
                }
            }
        }
    }

    pub async fn try_request_handle(self: &Arc<Self>) -> Result<ActiveRequestHandle, Duration> {
        // check that we are connected
        if !self.has_provider().await {
            // TODO: how long? use the same amount as the exponential backoff on retry
            return Err(Duration::from_secs(1));
        }

        // check rate limits
        if let Some(ratelimiter) = self.hard_limit.as_ref() {
            match ratelimiter.throttle().await {
                Ok(_) => {
                    // rate limit succeeded
                    return Ok(ActiveRequestHandle::new(self.clone()));
                }
                Err(retry_after) => {
                    // rate limit failed
                    // save the smallest retry_after. if nothing succeeds, return an Err with retry_after in it
                    // TODO: use tracing better
                    // TODO: i'm seeing "Exhausted rate limit on moralis: 0ns". How is it getting 0?
                    warn!("Exhausted rate limit on {:?}: {:?}", self, retry_after);

                    return Err(retry_after);
                }
            }
        };

        Ok(ActiveRequestHandle::new(self.clone()))
    }
}

impl Hash for Web3Connection {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.url.hash(state);
    }
}

/// Drop this once a connection completes
pub struct ActiveRequestHandle(Arc<Web3Connection>);

impl ActiveRequestHandle {
    fn new(connection: Arc<Web3Connection>) -> Self {
        // TODO: attach a unique id to this?
        // TODO: what ordering?!
        connection
            .active_requests
            .fetch_add(1, atomic::Ordering::AcqRel);

        Self(connection)
    }

    pub fn clone_connection(&self) -> Arc<Web3Connection> {
        self.0.clone()
    }

    /// Send a web3 request
    /// By having the request method here, we ensure that the rate limiter was called and connection counts were properly incremented
    /// By taking self here, we ensure that this is dropped after the request is complete
    #[instrument(skip_all)]
    pub async fn request<T, R>(
        &self,
        method: &str,
        params: T,
    ) -> Result<R, ethers::prelude::ProviderError>
    where
        T: fmt::Debug + serde::Serialize + Send + Sync,
        R: serde::Serialize + serde::de::DeserializeOwned + fmt::Debug,
    {
        // TODO: use tracing spans properly
        // TODO: it would be nice to have the request id on this
        // TODO: including params in this is way too verbose
        trace!("Sending {} to {}", method, self.0);

        let mut provider = None;

        while provider.is_none() {
            // TODO: if no provider, don't unwrap. wait until there is one.
            match self.0.provider.read().await.as_ref() {
                None => {}
                Some(found_provider) => provider = Some(found_provider.clone()),
            }
        }

        let response = match &*provider.unwrap() {
            Web3Provider::Http(provider) => provider.request(method, params).await,
            Web3Provider::Ws(provider) => provider.request(method, params).await,
        };

        // TODO: i think ethers already has trace logging (and does it much more fancy)
        // TODO: at least instrument this with more useful information
        // trace!("Reply from {}: {:?}", self.0, response);
        trace!("Reply from {}", self.0);

        response
    }
}

impl Drop for ActiveRequestHandle {
    fn drop(&mut self) {
        self.0
            .active_requests
            .fetch_sub(1, atomic::Ordering::AcqRel);
    }
}

impl Eq for Web3Connection {}

impl Ord for Web3Connection {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // TODO: what atomic ordering?!
        let a = self.active_requests.load(atomic::Ordering::Acquire);
        let b = other.active_requests.load(atomic::Ordering::Acquire);

        // TODO: how should we include the soft limit? floats are slower than integer math
        let a = (a + 1) as f32 / self.soft_limit as f32;
        let b = (b + 1) as f32 / other.soft_limit as f32;

        a.partial_cmp(&b).unwrap()
    }
}

impl PartialOrd for Web3Connection {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// note that this is just comparing the active requests. two providers with different rpc urls are equal!
impl PartialEq for Web3Connection {
    fn eq(&self, other: &Self) -> bool {
        // TODO: what ordering?!
        self.active_requests.load(atomic::Ordering::Acquire)
            == other.active_requests.load(atomic::Ordering::Acquire)
    }
}
