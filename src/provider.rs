///! Communicate with a web3 providers
use derive_more::From;
use ethers::prelude::{BlockNumber, Middleware};
use futures::StreamExt;
use std::time::Duration;
use std::{cmp::Ordering, sync::Arc};
use tokio::time::interval;
use tracing::{info, warn};

use crate::block_watcher::BlockWatcherSender;

// TODO: instead of an enum, I tried to use Box<dyn Provider>, but hit https://github.com/gakonst/ethers-rs/issues/592
#[derive(From, Debug)]
pub enum Web3Provider {
    Http(ethers::providers::Provider<ethers::providers::Http>),
    Ws(ethers::providers::Provider<ethers::providers::Ws>),
}

/// Forward functions to the inner ethers::providers::Provider
impl Web3Provider {
    /// Send a web3 request
    pub async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ethers::prelude::ProviderError> {
        match self {
            Self::Http(provider) => provider.request(method, params).await,
            Self::Ws(provider) => provider.request(method, params).await,
        }
    }

    /// Subscribe to new blocks
    pub async fn new_heads(
        &self,
        url: String,
        block_watcher_sender: BlockWatcherSender,
    ) -> anyhow::Result<()> {
        info!("Watching new_heads from {}", url);

        match &self {
            Web3Provider::Http(provider) => {
                // TODO: there is a "watch_blocks" function, but a lot of public nodes do not support the necessary rpc endpoints
                // TODO: what should this interval be?
                // TODO: maybe it would be better to have one interval for all of the http providers, but this works for now
                let mut interval = interval(Duration::from_secs(2));

                loop {
                    // wait for 2 seconds
                    interval.tick().await;

                    match provider.get_block(BlockNumber::Latest).await {
                        Ok(Some(block)) => block_watcher_sender.send((url.clone(), block)).unwrap(),
                        Ok(None) => warn!("no black at {}", url),
                        Err(e) => warn!("getBlock at {} failed: {}", url, e),
                    }
                }
            }
            Web3Provider::Ws(provider) => {
                // TODO: automatically reconnect?
                let mut stream = provider.subscribe_blocks().await?;
                while let Some(block) = stream.next().await {
                    block_watcher_sender.send((url.clone(), block)).unwrap();
                }
            }
        }

        info!("Done watching new_heads from {}", url);

        Ok(())
    }
}

/// An active connection to a Web3Rpc
#[derive(Debug)]
pub struct Web3Connection {
    /// keep track of currently open requests. We sort on this
    active_requests: u32,
    provider: Arc<Web3Provider>,
}

impl Web3Connection {
    pub fn clone_provider(&self) -> Arc<Web3Provider> {
        self.provider.clone()
    }

    /// Connect to a web3 rpc and subscribe to new heads
    pub async fn try_new(
        url_str: String,
        http_client: Option<reqwest::Client>,
        block_watcher_sender: BlockWatcherSender,
    ) -> anyhow::Result<Web3Connection> {
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

        let provider = Arc::new(provider);

        // subscribe to new heads in a spawned future
        let provider_clone: Arc<Web3Provider> = Arc::clone(&provider);
        tokio::spawn(async move {
            while let Err(e) = provider_clone
                .new_heads(url_str.clone(), block_watcher_sender.clone())
                .await
            {
                warn!("new_heads error for {}: {:?}", url_str, e);
            }
        });

        Ok(Web3Connection {
            active_requests: 0,
            provider,
        })
    }

    pub fn inc_active_requests(&mut self) {
        self.active_requests += 1;
    }

    pub fn dec_active_requests(&mut self) {
        self.active_requests -= 1;
    }
}

impl Eq for Web3Connection {}

impl Ord for Web3Connection {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.active_requests.cmp(&other.active_requests)
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
        self.active_requests == other.active_requests
    }
}
