use derive_more::From;
use ethers::prelude::Middleware;
use futures::StreamExt;
use std::time::Duration;
use std::{cmp::Ordering, sync::Arc};
use tracing::{info, warn};

use crate::block_watcher::{BlockWatcherItem, BlockWatcherSender};

// TODO: instead of an enum, I tried to use Box<dyn Provider>, but hit https://github.com/gakonst/ethers-rs/issues/592
#[derive(From)]
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

        // TODO: automatically reconnect
        match &self {
            Web3Provider::Http(_provider) => {
                /*
                // TODO: not all providers have this. we need to write our interval checking
                let mut stream = provider.watch_blocks().await?;
                while let Some(block_number) = stream.next().await {
                    let block = provider.get_block(block_number).await?.expect("no block");
                    block_watcher_sender
                        .send(Some((url.clone(), block)))
                        .unwrap();
                }
                */
                block_watcher_sender
                    .send(BlockWatcherItem::SubscribeHttp(url.clone()))
                    .unwrap();
            }
            Web3Provider::Ws(provider) => {
                let mut stream = provider.subscribe_blocks().await?;
                while let Some(block) = stream.next().await {
                    block_watcher_sender
                        .send(BlockWatcherItem::NewHead((url.clone(), block)))
                        .unwrap();
                }
            }
        }

        info!("Done watching new_heads from {}", url);

        Ok(())
    }
}

/// An active connection to a Web3Rpc
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
        // TODO: create an ethers-rs rpc client and subscribe/watch new heads in a spawned task
        let provider = if url_str.starts_with("http") {
            let url: url::Url = url_str.parse()?;

            let http_client = http_client.ok_or_else(|| anyhow::anyhow!("no http_client"))?;

            let provider = ethers::providers::Http::new_with_client(url, http_client);

            // TODO: dry this up
            ethers::providers::Provider::new(provider)
                .interval(Duration::from_secs(1))
                .into()
        } else if url_str.starts_with("ws") {
            let provider = ethers::providers::Ws::connect(url_str.clone()).await?;

            // TODO: make sure this survives disconnects

            // TODO: dry this up
            ethers::providers::Provider::new(provider)
                .interval(Duration::from_secs(1))
                .into()
        } else {
            return Err(anyhow::anyhow!("only http and ws servers are supported"));
        };

        let provider = Arc::new(provider);

        // subscribe to new heads in a spawned future
        // TODO: if http, maybe we should check them all on the same interval. and if there is at least one websocket, use that message to start check?
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
