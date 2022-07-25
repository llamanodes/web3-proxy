use argh::FromArgs;
use derive_more::Constructor;
use ethers::prelude::{Block, TxHash};
use hashbrown::HashMap;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::app::AnyhowJoinHandle;
use crate::connection::Web3Connection;

pub type BlockAndRpc = (Arc<Block<TxHash>>, Arc<Web3Connection>);

#[derive(Debug, FromArgs)]
/// Web3-proxy is a fast caching and load balancing proxy for web3 (Ethereum or similar) JsonRPC servers.
pub struct CliConfig {
    /// what port the proxy should listen on
    #[argh(option, default = "8544")]
    pub port: u16,

    /// number of worker threads. Defaults to the number of logical processors
    #[argh(option, default = "0")]
    pub workers: usize,

    /// path to a toml of rpc servers
    #[argh(option, default = "\"./config/development.toml\".to_string()")]
    pub config: String,
}

#[derive(Debug, Deserialize)]
pub struct AppConfig {
    pub shared: RpcSharedConfig,
    pub balanced_rpcs: HashMap<String, Web3ConnectionConfig>,
    pub private_rpcs: Option<HashMap<String, Web3ConnectionConfig>>,
}

/// shared configuration between Web3Connections
#[derive(Debug, Deserialize)]
pub struct RpcSharedConfig {
    // TODO: better type for chain_id? max of `u64::MAX / 2 - 36` https://github.com/ethereum/EIPs/issues/2294
    pub chain_id: u64,
    pub redis_url: Option<String>,
    #[serde(default = "default_public_rate_limit_per_minute")]
    pub public_rate_limit_per_minute: u32,
    #[serde(default = "default_response_cache_max_bytes")]
    pub response_cache_max_bytes: usize,
}

fn default_public_rate_limit_per_minute() -> u32 {
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
    hard_limit: Option<u32>,
}

impl Web3ConnectionConfig {
    /// Create a Web3Connection from config
    // #[instrument(name = "try_build_Web3ConnectionConfig", skip_all)]
    pub async fn spawn(
        self,
        redis_client_pool: Option<redis_cell_client::RedisClientPool>,
        chain_id: u64,
        http_client: Option<reqwest::Client>,
        http_interval_sender: Option<Arc<broadcast::Sender<()>>>,
        block_sender: Option<flume::Sender<BlockAndRpc>>,
        tx_id_sender: Option<flume::Sender<(TxHash, Arc<Web3Connection>)>>,
    ) -> anyhow::Result<(Arc<Web3Connection>, AnyhowJoinHandle<()>)> {
        let hard_limit = match (self.hard_limit, redis_client_pool) {
            (None, None) => None,
            (Some(hard_limit), Some(redis_client_pool)) => Some((hard_limit, redis_client_pool)),
            (None, Some(_)) => None,
            (Some(hard_limit), None) => {
                return Err(anyhow::anyhow!(
                    "no redis client pool! needed for hard limit"
                ))
            }
        };

        Web3Connection::spawn(
            chain_id,
            self.url,
            http_client,
            http_interval_sender,
            hard_limit,
            self.soft_limit,
            block_sender,
            tx_id_sender,
            true,
        )
        .await
    }
}
