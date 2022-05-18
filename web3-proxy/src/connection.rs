///! Rate-limited communication with a web3 provider
use derive_more::From;
use ethers::prelude::{Block, Middleware, ProviderError, TxHash, H256};
use futures::StreamExt;
use governor::clock::{Clock, QuantaClock, QuantaInstant};
use governor::middleware::NoOpMiddleware;
use governor::state::{InMemoryState, NotKeyed};
use governor::NotUntil;
use governor::RateLimiter;
use std::fmt;
use std::num::NonZeroU32;
use std::sync::atomic::{self, AtomicU32};
use std::{cmp::Ordering, sync::Arc};
use tokio::sync::RwLock;
use tokio::task;
use tokio::time::{interval, sleep, timeout_at, Duration, Instant, MissedTickBehavior};
use tracing::{info, instrument, trace, warn};

type Web3RateLimiter =
    RateLimiter<NotKeyed, InMemoryState, QuantaClock, NoOpMiddleware<QuantaInstant>>;

/// TODO: instead of an enum, I tried to use Box<dyn Provider>, but hit https://github.com/gakonst/ethers-rs/issues/592
#[derive(From)]
pub enum Web3Provider {
    Http(ethers::providers::Provider<ethers::providers::Http>),
    Ws(ethers::providers::Provider<ethers::providers::Ws>),
}

impl Web3Provider {
    #[instrument]
    async fn from_str(url_str: &str, http_client: Option<reqwest::Client>) -> anyhow::Result<Self> {
        let provider = if url_str.starts_with("http") {
            let url: url::Url = url_str.parse()?;

            let http_client = http_client.ok_or_else(|| anyhow::anyhow!("no http_client"))?;

            let provider = ethers::providers::Http::new_with_client(url, http_client);

            // TODO: dry this up (needs https://github.com/gakonst/ethers-rs/issues/592)
            ethers::providers::Provider::new(provider)
                .interval(Duration::from_secs(1))
                .into()
        } else if url_str.starts_with("ws") {
            // TODO: wrapper automatically reconnect
            let provider = ethers::providers::Ws::connect(url_str).await?;

            // TODO: make sure this automatically reconnects

            // TODO: dry this up (needs https://github.com/gakonst/ethers-rs/issues/592)
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
    // TODO: put this in a RwLock so that we can replace it if re-connecting
    provider: RwLock<Arc<Web3Provider>>,
    ratelimiter: Option<Web3RateLimiter>,
    /// used for load balancing to the least loaded server
    soft_limit: u32,
    /// the same clock that is used by the rate limiter
    clock: QuantaClock,
}

impl fmt::Debug for Web3Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Web3Connection")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for Web3Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", &self.url)
    }
}

impl Web3Connection {
    #[instrument(skip_all)]
    pub async fn reconnect(
        self: &Arc<Self>,
        block_sender: &flume::Sender<(u64, H256, usize)>,
        rpc_id: usize,
    ) -> anyhow::Result<()> {
        // websocket doesn't need the http client
        let http_client = None;

        // since this lock is held open over an await, we use tokio's locking
        let mut provider = self.provider.write().await;

        // TODO: tell the block subscriber that we are at 0
        block_sender
            .send_async((0, H256::default(), rpc_id))
            .await?;

        let new_provider = Web3Provider::from_str(&self.url, http_client).await?;

        *provider = Arc::new(new_provider);

        Ok(())
    }

    /// Connect to a web3 rpc and subscribe to new heads
    #[instrument(name = "try_new_Web3Connection", skip(clock, http_client))]
    pub async fn try_new(
        chain_id: usize,
        url_str: String,
        // optional because this is only used for http providers. websocket providers don't use it
        http_client: Option<reqwest::Client>,
        hard_rate_limit: Option<u32>,
        clock: &QuantaClock,
        // TODO: think more about this type
        soft_limit: u32,
    ) -> anyhow::Result<Arc<Web3Connection>> {
        let hard_rate_limiter = if let Some(hard_rate_limit) = hard_rate_limit {
            let quota = governor::Quota::per_second(NonZeroU32::new(hard_rate_limit).unwrap());

            let rate_limiter = governor::RateLimiter::direct_with_clock(quota, clock);

            Some(rate_limiter)
        } else {
            None
        };

        let provider = Web3Provider::from_str(&url_str, http_client).await?;

        let connection = Web3Connection {
            clock: clock.clone(),
            url: url_str.clone(),
            active_requests: 0.into(),
            provider: RwLock::new(Arc::new(provider)),
            ratelimiter: hard_rate_limiter,
            soft_limit,
        };

        let connection = Arc::new(connection);

        // check the server's chain_id here
        let active_request_handle = connection.wait_for_request_handle().await;
        // TODO: some public rpcs (on bsc and fantom) do not return an id and so this ends up being an error
        let found_chain_id: Result<String, _> = active_request_handle
            .request("eth_chainId", Option::None::<()>)
            .await;

        match found_chain_id {
            Ok(found_chain_id) => {
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
                let e = anyhow::Error::from(e).context(format!("{}", connection));
                return Err(e);
            }
        }

        info!("Successful connection: {}", connection);

        Ok(connection)
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
    pub fn url(&self) -> &str {
        &self.url
    }

    #[instrument(skip_all)]
    async fn send_block(
        self: &Arc<Self>,
        block: Result<Block<TxHash>, ProviderError>,
        block_sender: &flume::Sender<(u64, H256, usize)>,
        rpc_id: usize,
    ) {
        match block {
            Ok(block) => {
                let block_number = block.number.unwrap().as_u64();
                let block_hash = block.hash.unwrap();

                // TODO: i'm pretty sure we don't need send_async, but double check
                block_sender
                    .send_async((block_number, block_hash, rpc_id))
                    .await
                    .unwrap();
            }
            Err(e) => {
                warn!("unable to get block from {}: {}", self, e);
            }
        }
    }

    /// Subscribe to new blocks. If `reconnect` is true, this runs forever.
    /// TODO: instrument with the url
    #[instrument(skip_all)]
    pub async fn subscribe_new_heads(
        self: Arc<Self>,
        rpc_id: usize,
        block_sender: flume::Sender<(u64, H256, usize)>,
        reconnect: bool,
    ) -> anyhow::Result<()> {
        loop {
            info!("Watching new_heads on {}", self);

            // TODO: is a RwLock of Arc the right thing here?
            let provider = self.provider.read().await.clone();

            match &*provider {
                Web3Provider::Http(provider) => {
                    // there is a "watch_blocks" function, but a lot of public nodes do not support the necessary rpc endpoints
                    // TODO: what should this interval be? probably some fraction of block time. set automatically?
                    // TODO: maybe it would be better to have one interval for all of the http providers, but this works for now
                    // TODO: if there are some websocket providers, maybe have a longer interval and a channel that tells the https to update when a websocket gets a new head? if they are slow this wouldn't work well though
                    let mut interval = interval(Duration::from_secs(2));
                    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

                    let mut last_hash = Default::default();

                    loop {
                        // wait for the interval
                        // TODO: if error or rate limit, increase interval?
                        interval.tick().await;

                        match self.try_request_handle() {
                            Ok(active_request_handle) => {
                                // TODO: i feel like this should be easier. there is a provider.getBlock, but i don't know how to give it "latest"
                                let block: Result<Block<TxHash>, _> = provider
                                    .request("eth_getBlockByNumber", ("latest", false))
                                    .await;

                                drop(active_request_handle);

                                // don't send repeat blocks
                                if let Ok(block) = &block {
                                    let new_hash = block.hash.unwrap();

                                    if new_hash == last_hash {
                                        continue;
                                    }

                                    last_hash = new_hash;
                                }

                                self.send_block(block, &block_sender, rpc_id).await;
                            }
                            Err(e) => {
                                warn!("Failed getting latest block from {}: {:?}", self, e);
                            }
                        }
                    }
                }
                Web3Provider::Ws(provider) => {
                    // rate limits
                    let active_request_handle = self.wait_for_request_handle().await;

                    // TODO: automatically reconnect?
                    // TODO: it would be faster to get the block number, but subscriptions don't provide that
                    // TODO: maybe we can do provider.subscribe("newHeads") and then parse into a custom struct that only gets the number out?
                    let mut stream = provider.subscribe_blocks().await?;

                    drop(active_request_handle);
                    let active_request_handle = self.wait_for_request_handle().await;

                    // query the block once since the subscription doesn't send the current block
                    // there is a very small race condition here where the stream could send us a new block right now
                    // all it does is print "new block" for the same block as current block
                    // TODO: rate limit!
                    let block: Result<Block<TxHash>, _> = active_request_handle
                        .request("eth_getBlockByNumber", ("latest", false))
                        .await;

                    self.send_block(block, &block_sender, rpc_id).await;

                    // TODO: what should this timeout be? needs to be larger than worst case block time
                    // TODO: although reconnects will make this less of an issue
                    loop {
                        match timeout_at(Instant::now() + Duration::from_secs(300), stream.next())
                            .await
                        {
                            Ok(Some(new_block)) => {
                                self.send_block(Ok(new_block), &block_sender, rpc_id).await;

                                // TODO: really not sure about this
                                task::yield_now().await;
                            }
                            Ok(None) => {
                                warn!("subscription ended");
                                break;
                            }
                            Err(e) => {
                                warn!("subscription ended with an error: {:?}", e);
                                break;
                            }
                        }
                    }
                }
            }

            if reconnect {
                drop(provider);

                // TODO: exponential backoff
                warn!("new heads subscription exited. reconnecting in 10 seconds...");
                sleep(Duration::from_secs(10)).await;

                self.reconnect(&block_sender, rpc_id).await?;
            } else {
                break;
            }
        }

        info!("Done watching new_heads on {}", self);
        Ok(())
    }

    #[instrument(skip_all)]
    pub async fn wait_for_request_handle(self: &Arc<Self>) -> ActiveRequestHandle {
        // TODO: maximum wait time

        for _ in 0..10 {
            match self.try_request_handle() {
                Ok(pending_request_handle) => return pending_request_handle,
                Err(not_until) => {
                    let deadline = not_until.wait_time_from(self.clock.now());

                    sleep(deadline).await;
                }
            }
        }

        // TODO: what should we do?
        panic!("no request handle after 10 tries");
    }

    pub fn try_request_handle(
        self: &Arc<Self>,
    ) -> Result<ActiveRequestHandle, NotUntil<QuantaInstant>> {
        // check rate limits
        if let Some(ratelimiter) = self.ratelimiter.as_ref() {
            match ratelimiter.check() {
                Ok(_) => {
                    // rate limit succeeded
                    return Ok(ActiveRequestHandle::new(self.clone()));
                }
                Err(not_until) => {
                    // rate limit failed
                    // save the smallest not_until. if nothing succeeds, return an Err with not_until in it
                    // TODO: use tracing better
                    warn!("Exhausted rate limit on {:?}: {}", self, not_until);

                    return Err(not_until);
                }
            }
        };

        Ok(ActiveRequestHandle::new(self.clone()))
    }
}

/// Drop this once a connection completes
pub struct ActiveRequestHandle(Arc<Web3Connection>);

impl ActiveRequestHandle {
    fn new(connection: Arc<Web3Connection>) -> Self {
        // TODO: attach a unique id to this
        // TODO: what ordering?!
        connection
            .active_requests
            .fetch_add(1, atomic::Ordering::AcqRel);

        Self(connection)
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

        let provider = self.0.provider.read().await.clone();

        let response = match &*provider {
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
        let a = a as f32 / self.soft_limit as f32;
        let b = b as f32 / other.soft_limit as f32;

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
