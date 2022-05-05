///! Communicate with a web3 provider
use derive_more::From;
use ethers::prelude::Middleware;
use futures::StreamExt;
use governor::clock::{QuantaClock, QuantaInstant};
use governor::middleware::NoOpMiddleware;
use governor::state::{InMemoryState, NotKeyed};
use governor::NotUntil;
use governor::RateLimiter;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::fmt;
use std::num::NonZeroU32;
use std::sync::atomic::{self, AtomicU32, AtomicU64};
use std::time::Duration;
use std::{cmp::Ordering, sync::Arc};
use tokio::time::interval;
use tracing::{info, warn};

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
        url_str: String,
        http_client: Option<reqwest::Client>,
        hard_rate_limit: Option<u32>,
        clock: Option<&QuantaClock>,
        // TODO: think more about this type
        soft_limit: u32,
    ) -> anyhow::Result<Web3Connection> {
        let hard_rate_limiter = if let Some(hard_rate_limit) = hard_rate_limit {
            let quota = governor::Quota::per_second(NonZeroU32::new(hard_rate_limit).unwrap());

            let rate_limiter = governor::RateLimiter::direct_with_clock(quota, clock.unwrap());

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
            let provider = ethers::providers::Ws::connect(url_str.clone()).await?;

            // TODO: make sure this automatically reconnects

            // TODO: dry this up (needs https://github.com/gakonst/ethers-rs/issues/592)
            ethers::providers::Provider::new(provider)
                .interval(Duration::from_secs(1))
                .into()
        } else {
            return Err(anyhow::anyhow!("only http and ws servers are supported"));
        };

        Ok(Web3Connection {
            url: url_str.clone(),
            active_requests: Default::default(),
            provider,
            ratelimiter: hard_rate_limiter,
            soft_limit,
            head_block_number: 0.into(),
        })
    }

    pub fn active_requests(&self) -> u32 {
        self.active_requests.load(atomic::Ordering::Acquire)
    }

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
                // TODO: what should this interval be? probably some fraction of block time
                // TODO: maybe it would be better to have one interval for all of the http providers, but this works for now
                let mut interval = interval(Duration::from_secs(2));

                loop {
                    // wait for the interval
                    interval.tick().await;

                    let block_number = provider.get_block_number().await.map(|x| x.as_u64())?;

                    // TODO: only store if this isn't already stored?
                    // TODO: also send something to the provider_tier so it can sort?
                    let old_block_number = self
                        .head_block_number
                        .swap(block_number, atomic::Ordering::AcqRel);

                    if old_block_number != block_number {
                        info!("new block on {}: {}", self, block_number);

                        if let Some(connections) = &connections {
                            connections.update_synced_rpcs(&self, block_number)?;
                        }
                    }
                }
            }
            Web3Provider::Ws(provider) => {
                // TODO: automatically reconnect?
                // TODO: it would be faster to get the block number, but subscriptions don't provide that
                // TODO: maybe we can do provider.subscribe("newHeads") and then parse into a custom struct that only gets the number out?
                let mut stream = provider.subscribe_blocks().await?;

                // query the block once since the subscription doesn't send the current block
                // there is a very small race condition here where the stream could send us a new block right now
                // all it does is print "new block" for the same block as current block
                let block_number = provider.get_block_number().await.map(|x| x.as_u64())?;

                info!("current block on {}: {}", self, block_number);

                self.head_block_number
                    .store(block_number, atomic::Ordering::Release);

                if let Some(connections) = &connections {
                    connections.update_synced_rpcs(&self, block_number)?;
                }

                while let Some(block) = stream.next().await {
                    let block_number = block.number.unwrap().as_u64();

                    // TODO: only store if this isn't already stored?
                    // TODO: also send something to the provider_tier so it can sort?
                    // TODO: do we need this old block number check? its helpful on http, but here it shouldn't dupe except maybe on the first run
                    self.head_block_number
                        .store(block_number, atomic::Ordering::Release);

                    info!("new block on {}: {}", self, block_number);

                    if let Some(connections) = &connections {
                        connections.update_synced_rpcs(&self, block_number)?;
                    }
                }
            }
        }

        info!("Done watching new_heads on {}", self);

        Ok(())
    }

    /// Send a web3 request
    pub async fn request(
        &self,
        method: &str,
        params: &serde_json::value::RawValue,
    ) -> Result<JsonRpcForwardedResponse, ethers::prelude::ProviderError> {
        match &self.provider {
            Web3Provider::Http(provider) => provider.request(method, params).await,
            Web3Provider::Ws(provider) => provider.request(method, params).await,
        }
    }

    pub fn try_inc_active_requests(&self) -> Result<(), NotUntil<QuantaInstant>> {
        // check rate limits
        if let Some(ratelimiter) = self.ratelimiter.as_ref() {
            match ratelimiter.check() {
                Ok(_) => {
                    // rate limit succeeded
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

        // TODO: what ordering?!
        self.active_requests.fetch_add(1, atomic::Ordering::AcqRel);

        Ok(())
    }

    pub fn dec_active_requests(&self) {
        // TODO: what ordering?!
        self.active_requests.fetch_sub(1, atomic::Ordering::AcqRel);
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

#[derive(Clone, Deserialize)]
pub struct JsonRpcRequest {
    pub id: Box<RawValue>,
    pub method: String,
    pub params: Box<RawValue>,
}

impl fmt::Debug for JsonRpcRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("JsonRpcRequest")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

// TODO: check for errors too!
#[derive(Clone, Deserialize, Serialize)]
pub struct JsonRpcForwardedResponse {
    pub id: Box<RawValue>,
    pub result: Box<RawValue>,
}

impl fmt::Debug for JsonRpcForwardedResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("JsonRpcForwardedResponse")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}
