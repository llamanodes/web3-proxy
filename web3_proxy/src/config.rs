use crate::rpcs::blockchain::BlockHashesCache;
use crate::rpcs::connection::Web3Connection;
use crate::rpcs::request::OpenRequestHandleMetrics;
use crate::{app::AnyhowJoinHandle, rpcs::blockchain::ArcBlock};
use argh::FromArgs;
use derive_more::Constructor;
use ethers::prelude::TxHash;
use hashbrown::HashMap;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::broadcast;

pub type BlockAndRpc = (Option<ArcBlock>, Arc<Web3Connection>);
pub type TxHashAndRpc = (TxHash, Arc<Web3Connection>);

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

    /// path to a binary file used to encrypt cookies. Should be at least 64 bytes.
    #[argh(option, default = "\"./data/development_cookie_key\".to_string()")]
    pub cookie_key_filename: String,
}

#[derive(Debug, Deserialize)]
pub struct TopConfig {
    pub app: AppConfig,
    pub balanced_rpcs: HashMap<String, Web3ConnectionConfig>,
    pub private_rpcs: Option<HashMap<String, Web3ConnectionConfig>>,
}

/// shared configuration between Web3Connections
// TODO: no String, only &str
#[derive(Debug, Default, Deserialize)]
pub struct AppConfig {
    // TODO: better type for chain_id? max of `u64::MAX / 2 - 36` https://github.com/ethereum/EIPs/issues/2294
    pub chain_id: u64,
    pub cookie_domain: Option<String>,
    pub cookie_secure: Option<bool>,
    pub db_url: Option<String>,
    /// minimum size of the connection pool for the database
    /// If none, the number of workers are used
    pub db_min_connections: Option<u32>,
    /// minimum size of the connection pool for the database
    /// If none, the minimum * 2 is used
    pub db_max_connections: Option<u32>,
    pub influxdb_url: Option<String>,
    pub influxdb_name: Option<String>,
    pub default_requests_per_minute: Option<u64>,
    pub invite_code: Option<String>,
    #[serde(default = "default_min_sum_soft_limit")]
    pub min_sum_soft_limit: u32,
    #[serde(default = "default_min_synced_rpcs")]
    pub min_synced_rpcs: usize,
    /// Set to 0 to block all anonymous requests
    #[serde(default = "default_frontend_rate_limit_per_minute")]
    pub frontend_rate_limit_per_minute: u64,
    #[serde(default = "default_login_rate_limit_per_minute")]
    pub login_rate_limit_per_minute: u64,
    pub redis_url: Option<String>,
    /// maximum size of the connection pool for the cache
    /// If none, the minimum * 2 is used
    pub redis_max_connections: Option<usize>,
    #[serde(default = "default_response_cache_max_bytes")]
    pub response_cache_max_bytes: usize,
    /// the stats page url for an anonymous user.
    pub redirect_public_url: String,
    /// the stats page url for a logged in user. it must contain "{user_id}"
    pub redirect_user_url: String,
}

fn default_min_sum_soft_limit() -> u32 {
    1
}

fn default_min_synced_rpcs() -> usize {
    1
}

/// 0 blocks anonymous requests by default.
fn default_frontend_rate_limit_per_minute() -> u64 {
    0
}

/// Having a low amount of requests per minute for login is safest.
fn default_login_rate_limit_per_minute() -> u64 {
    10
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
    subscribe_txs: Option<bool>,
}

impl Web3ConnectionConfig {
    /// Create a Web3Connection from config
    /// TODO: move this into Web3Connection (just need to make things pub(crate))
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn(
        self,
        name: String,
        redis_pool: Option<redis_rate_limiter::RedisPool>,
        chain_id: u64,
        http_client: Option<reqwest::Client>,
        http_interval_sender: Option<Arc<broadcast::Sender<()>>>,
        block_map: BlockHashesCache,
        block_sender: Option<flume::Sender<BlockAndRpc>>,
        tx_id_sender: Option<flume::Sender<TxHashAndRpc>>,
        open_request_handle_metrics: Arc<OpenRequestHandleMetrics>,
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

        let tx_id_sender = if self.subscribe_txs.unwrap_or(false) {
            tx_id_sender
        } else {
            None
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
            open_request_handle_metrics,
        )
        .await
    }
}
