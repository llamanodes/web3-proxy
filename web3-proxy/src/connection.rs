///! Rate-limited communication with a web3 provider
use derive_more::From;
use ethers::prelude::Middleware;
use futures::StreamExt;
use governor::clock::{Clock, QuantaClock, QuantaInstant};
use governor::middleware::NoOpMiddleware;
use governor::state::{InMemoryState, NotKeyed};
use governor::NotUntil;
use governor::RateLimiter;
use std::fmt;
use std::num::NonZeroU32;
use std::sync::atomic::{self, AtomicU32, AtomicU64};
use std::time::Duration;
use std::{cmp::Ordering, sync::Arc};
use tokio::time::{interval, sleep, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::connections::Web3Connections;

type Web3RateLimiter =
    RateLimiter<NotKeyed, InMemoryState, QuantaClock, NoOpMiddleware<QuantaInstant>>;

/// TODO: instead of an enum, I tried to use Box<dyn Provider>, but hit https://github.com/gakonst/ethers-rs/issues/592
#[derive(From)]
pub enum Web3Provider {
    Http(ethers::providers::Provider<ethers::providers::Http>),
    Ws(ethers::providers::Provider<ethers::providers::Ws>),
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
    provider: Web3Provider,
    ratelimiter: Option<Web3RateLimiter>,
    /// used for load balancing to the least loaded server
    soft_limit: u32,
    head_block_number: AtomicU64,
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
    /// Connect to a web3 rpc and subscribe to new heads
    pub async fn try_new(
        chain_id: usize,
        url_str: String,
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
            let provider = ethers::providers::Ws::connect(url_str.clone()).await?;

            // TODO: make sure this automatically reconnects

            // TODO: dry this up (needs https://github.com/gakonst/ethers-rs/issues/592)
            ethers::providers::Provider::new(provider)
                .interval(Duration::from_secs(1))
                .into()
        } else {
            return Err(anyhow::anyhow!("only http and ws servers are supported"));
        };

        let connection = Web3Connection {
            clock: clock.clone(),
            url: url_str.clone(),
            active_requests: Default::default(),
            provider,
            ratelimiter: hard_rate_limiter,
            soft_limit,
            head_block_number: 0.into(),
        };

        let connection = Arc::new(connection);

        // TODO: check the chain_id here
        let active_request_handle = connection.wait_for_request_handle().await;

        // TODO: passing empty_params like this feels awkward.
        let empty_params: Option<()> = None;
        let found_chain_id: String = active_request_handle
            .request("eth_chainId", empty_params)
            .await
            .unwrap();

        let found_chain_id =
            usize::from_str_radix(found_chain_id.trim_start_matches("0x"), 16).unwrap();

        if chain_id != found_chain_id {
            return Err(anyhow::anyhow!(
                "incorrect chain id! Expected {}. Found {}",
                chain_id,
                found_chain_id
            ));
        }

        info!("Successful connection: {}", connection);

        Ok(connection)
    }

    #[inline]
    pub fn active_requests(&self) -> u32 {
        self.active_requests.load(atomic::Ordering::Acquire)
    }

    #[inline]
    pub fn head_block_number(&self) -> u64 {
        self.head_block_number.load(atomic::Ordering::Acquire)
    }

    #[inline]
    pub fn soft_limit(&self) -> u32 {
        self.soft_limit
    }

    #[inline]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Subscribe to new blocks
    // #[instrument]
    pub async fn new_heads(
        self: Arc<Self>,
        connections: Option<Arc<Web3Connections>>,
    ) -> anyhow::Result<()> {
        info!("Watching new_heads on {}", self);

        match &self.provider {
            Web3Provider::Http(provider) => {
                // there is a "watch_blocks" function, but a lot of public nodes do not support the necessary rpc endpoints
                // TODO: what should this interval be? probably some fraction of block time. set automatically?
                // TODO: maybe it would be better to have one interval for all of the http providers, but this works for now
                let mut interval = interval(Duration::from_secs(2));
                interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

                loop {
                    // wait for the interval
                    // TODO: if error or rate limit, increase interval?
                    interval.tick().await;

                    let active_request_handle = self.wait_for_request_handle().await;

                    let block_number = provider.get_block_number().await.map(|x| x.as_u64())?;

                    drop(active_request_handle);

                    // TODO: only store if this isn't already stored?
                    // TODO: also send something to the provider_tier so it can sort?
                    let old_block_number = self
                        .head_block_number
                        .swap(block_number, atomic::Ordering::AcqRel);

                    if old_block_number != block_number {
                        if let Some(connections) = &connections {
                            connections.update_synced_rpcs(&self)?;
                        } else {
                            info!("new block on {}: {}", self, block_number);
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
                let block_number = provider.get_block_number().await.map(|x| x.as_u64())?;

                drop(active_request_handle);

                // TODO: swap and check the result?
                self.head_block_number
                    .store(block_number, atomic::Ordering::Release);

                if let Some(connections) = &connections {
                    connections.update_synced_rpcs(&self)?;
                } else {
                    info!("new head block {} from {}", block_number, self);
                }

                while let Some(new_block) = stream.next().await {
                    let new_block_number = new_block.number.unwrap().as_u64();

                    // TODO: only store if this isn't already stored?
                    // TODO: also send something to the provider_tier so it can sort?
                    // TODO: do we need this old block number check? its helpful on http, but here it shouldn't dupe except maybe on the first run
                    self.head_block_number
                        .fetch_max(new_block_number, atomic::Ordering::AcqRel);

                    if let Some(connections) = &connections {
                        connections.update_synced_rpcs(&self)?;
                    }
                }
            }
        }

        info!("Done watching new_heads on {}", self);

        Ok(())
    }

    pub async fn wait_for_request_handle(self: &Arc<Self>) -> ActiveRequestHandle {
        loop {
            match self.try_request_handle() {
                Ok(pending_request_handle) => return pending_request_handle,
                Err(not_until) => {
                    let deadline = not_until.wait_time_from(self.clock.now());

                    sleep(deadline).await;
                }
            }
        }
    }

    pub fn try_request_handle(
        self: &Arc<Self>,
    ) -> Result<ActiveRequestHandle, NotUntil<QuantaInstant>> {
        // check rate limits
        if let Some(ratelimiter) = self.ratelimiter.as_ref() {
            match ratelimiter.check() {
                Ok(_) => {
                    // rate limit succeeded
                    return Ok(ActiveRequestHandle(self.clone()));
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
    pub async fn request<T, R>(
        &self,
        method: &str,
        params: T,
    ) -> Result<R, ethers::prelude::ProviderError>
    where
        T: fmt::Debug + serde::Serialize + Send + Sync,
        R: serde::Serialize + serde::de::DeserializeOwned + fmt::Debug,
    {
        // TODO: this should probably be trace level and use a span
        // TODO: it would be nice to have the request id on this
        debug!("Sending {}({:?}) to {}", method, params, self.0);

        let response = match &self.0.provider {
            Web3Provider::Http(provider) => provider.request(method, params).await,
            Web3Provider::Ws(provider) => provider.request(method, params).await,
        };

        // TODO: i think ethers already has trace logging (and does it much more fancy)
        // TODO: at least instrument this with more useful information
        debug!("Response from {}: {:?}", self.0, response);

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
