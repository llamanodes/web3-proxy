use ethers::prelude::{Block, TxHash};
use governor::clock::QuantaClock;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroU32;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::block_watcher::BlockWatcherSender;
// use crate::block_watcher::BlockWatcher;
use crate::connection::Web3Connection;
use crate::Web3ProxyApp;

#[derive(Deserialize)]
pub struct RootConfig {
    pub config: Web3ProxyConfig,
    pub balanced_rpc_tiers: BTreeMap<String, HashMap<String, Web3ConnectionConfig>>,
    pub private_rpcs: HashMap<String, Web3ConnectionConfig>,
}

#[derive(Deserialize)]
pub struct Web3ProxyConfig {
    pub listen_port: u16,
}

#[derive(Deserialize)]
pub struct Web3ConnectionConfig {
    url: String,
    soft_limit: u32,
    hard_limit: Option<u32>,
}

impl RootConfig {
    pub async fn try_build(self) -> anyhow::Result<Web3ProxyApp> {
        let balanced_rpc_tiers = self
            .balanced_rpc_tiers
            .into_values()
            .map(|x| x.into_values().collect())
            .collect();
        let private_rpcs = self.private_rpcs.into_values().collect();

        Web3ProxyApp::try_new(balanced_rpc_tiers, private_rpcs).await
    }
}

impl Web3ConnectionConfig {
    pub async fn try_build(
        self,
        clock: &QuantaClock,
        block_watcher_sender: BlockWatcherSender,
        http_client: Option<reqwest::Client>,
    ) -> anyhow::Result<Arc<Web3Connection>> {
        let hard_rate_limiter = if let Some(hard_limit) = self.hard_limit {
            let quota = governor::Quota::per_second(NonZeroU32::new(hard_limit).unwrap());

            let rate_limiter = governor::RateLimiter::direct_with_clock(quota, clock);

            Some(rate_limiter)
        } else {
            None
        };

        Web3Connection::try_new(
            self.url,
            http_client,
            block_watcher_sender,
            hard_rate_limiter,
            self.soft_limit,
        )
        .await
    }

    pub fn url(&self) -> &str {
        &self.url
    }
}
