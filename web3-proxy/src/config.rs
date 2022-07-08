use argh::FromArgs;
use ethers::prelude::{Block, TxHash};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::app::AnyhowJoinHandle;
use crate::connection::Web3Connection;

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
    /// TODO: what type for chain_id? TODO: this isn't at the right level. this is inside a "Config"
    pub chain_id: usize,
    pub rate_limit_redis: Option<String>,
    // TODO: serde default for development?
    // TODO: allow no limit?
    pub public_rate_limit_per_minute: u32,
}

#[derive(Debug, Deserialize)]
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
        redis_client_pool: Option<&redis_cell_client::RedisClientPool>,
        chain_id: usize,
        http_client: Option<&reqwest::Client>,
        http_interval_sender: Option<Arc<broadcast::Sender<()>>>,
        block_sender: Option<flume::Sender<(Block<TxHash>, Arc<Web3Connection>)>>,
        tx_id_sender: Option<flume::Sender<(TxHash, Arc<Web3Connection>)>>,
    ) -> anyhow::Result<(Arc<Web3Connection>, AnyhowJoinHandle<()>)> {
        let hard_rate_limit = self.hard_limit.map(|x| (x, redis_client_pool.unwrap()));

        Web3Connection::spawn(
            chain_id,
            self.url,
            http_client,
            http_interval_sender,
            hard_rate_limit,
            self.soft_limit,
            block_sender,
            tx_id_sender,
            true,
        )
        .await
    }
}
