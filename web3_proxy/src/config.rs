use crate::app::AnyhowJoinHandle;
use crate::rpcs::connection::Web3Connection;
use crate::rpcs::connections::BlockHashesMap;
use argh::FromArgs;
use derive_more::Constructor;
use ethers::prelude::{Block, TxHash};
use hashbrown::HashMap;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::broadcast;

pub type BlockAndRpc = (Arc<Block<TxHash>>, Arc<Web3Connection>);

#[derive(Debug, FromArgs)]
/// Web3_proxy is a fast caching and load balancing proxy for web3 (Ethereum or similar) JsonRPC servers.
pub struct CliConfig {
    /// path to a toml of rpc servers
    #[argh(option, default = "\"./config/development.toml\".to_string()")]
    pub config: String,

    /// what port the proxy should listen on
    #[argh(option, default = "8544")]
    pub port: u16,

    /// what port the proxy should expose prometheus stats on
    #[argh(option, default = "8543")]
    pub prometheus_port: u16,

    /// number of worker threads. Defaults to the number of logical processors
    #[argh(option, default = "0")]
    pub workers: usize,
}

#[derive(Debug, Deserialize)]
pub struct TopConfig {
    pub app: AppConfig,
    pub balanced_rpcs: HashMap<String, Web3ConnectionConfig>,
    pub private_rpcs: Option<HashMap<String, Web3ConnectionConfig>>,
}

/// shared configuration between Web3Connections
#[derive(Debug, Deserialize)]
pub struct AppConfig {
    // TODO: better type for chain_id? max of `u64::MAX / 2 - 36` https://github.com/ethereum/EIPs/issues/2294
    pub chain_id: u64,
    pub db_url: Option<String>,
    pub invite_code: Option<String>,
    #[serde(default = "default_default_requests_per_minute")]
    pub default_requests_per_minute: u32,
    #[serde(default = "default_min_sum_soft_limit")]
    pub min_sum_soft_limit: u32,
    #[serde(default = "default_min_synced_rpcs")]
    pub min_synced_rpcs: u32,
    pub redis_url: Option<String>,
    #[serde(default = "default_public_rate_limit_per_minute")]
    pub public_rate_limit_per_minute: u64,
    #[serde(default = "default_response_cache_max_bytes")]
    pub response_cache_max_bytes: usize,
    /// the stats page url for an anonymous user.
    pub redirect_public_url: String,
    /// the stats page url for a logged in user. it must contain "{user_id}"
    pub redirect_user_url: String,
}

// TODO: what should we default to? have different tiers for different paid amounts?
fn default_default_requests_per_minute() -> u32 {
    1_000_000 * 60
}

fn default_min_sum_soft_limit() -> u32 {
    1
}

fn default_min_synced_rpcs() -> u32 {
    1
}

/// 0 blocks public requests by default.
fn default_public_rate_limit_per_minute() -> u64 {
    0
}

fn default_response_cache_max_bytes() -> usize {
    // TODO: default to some percentage of the system?
    // 100 megabytes
    10_usize.pow(8)
}

#[derive(Debug, Deserialize, Constructor)]
pub struct Web3ConnectionConfig {
    url: String,
    soft_limit: u32,
    hard_limit: Option<u64>,
    weight: u32,
}

impl Web3ConnectionConfig {
    /// Create a Web3Connection from config
    // #[instrument(name = "try_build_Web3ConnectionConfig", skip_all)]
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn(
        self,
        name: String,
        redis_pool: Option<redis_rate_limit::RedisPool>,
        chain_id: u64,
        http_client: Option<reqwest::Client>,
        http_interval_sender: Option<Arc<broadcast::Sender<()>>>,
        block_map: BlockHashesMap,
        block_sender: Option<flume::Sender<BlockAndRpc>>,
        tx_id_sender: Option<flume::Sender<(TxHash, Arc<Web3Connection>)>>,
    ) -> anyhow::Result<(Arc<Web3Connection>, AnyhowJoinHandle<()>)> {
        let hard_limit = match (self.hard_limit, redis_pool) {
            (None, None) => None,
            (Some(hard_limit), Some(redis_client_pool)) => Some((hard_limit, redis_client_pool)),
            (None, Some(_)) => None,
            (Some(_hard_limit), None) => {
                return Err(anyhow::anyhow!(
                    "no redis client pool! needed for hard limit"
                ))
            }
        };

        Web3Connection::spawn(
            name,
            chain_id,
            self.url,
            http_client,
            http_interval_sender,
            hard_limit,
            self.soft_limit,
            block_map,
            block_sender,
            tx_id_sender,
            true,
            self.weight,
        )
        .await
    }
}
