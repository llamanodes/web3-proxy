use crate::app::Web3ProxyJoinHandle;
use crate::rpcs::blockchain::{BlocksByHashCache, Web3ProxyBlock};
use crate::rpcs::one::Web3Rpc;
use argh::FromArgs;
use ethers::prelude::{Address, TxHash, H256};
use ethers::types::{U256, U64};
use hashbrown::HashMap;
use log::warn;
use migration::sea_orm::DatabaseConnection;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

pub type BlockAndRpc = (Option<Web3ProxyBlock>, Arc<Web3Rpc>);
pub type TxHashAndRpc = (TxHash, Arc<Web3Rpc>);

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

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct TopConfig {
    pub app: AppConfig,
    pub balanced_rpcs: HashMap<String, Web3RpcConfig>,
    pub private_rpcs: Option<HashMap<String, Web3RpcConfig>>,
    pub bundler_4337_rpcs: Option<HashMap<String, Web3RpcConfig>>,
    /// unknown config options get put here
    #[serde(flatten, default = "HashMap::default")]
    pub extra: HashMap<String, serde_json::Value>,
}

/// shared configuration between Web3Rpcs
// TODO: no String, only &str
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    /// Request limit for allowed origins for anonymous users.
    /// These requests get rate limited by IP.
    #[serde(default = "default_allowed_origin_requests_per_period")]
    pub allowed_origin_requests_per_period: HashMap<String, u64>,

    /// erigon defaults to pruning beyond 90,000 blocks
    #[serde(default = "default_archive_depth")]
    pub archive_depth: u64,

    /// EVM chain id. 1 for ETH
    /// TODO: better type for chain_id? max of `u64::MAX / 2 - 36` <https://github.com/ethereum/EIPs/issues/2294>
    pub chain_id: u64,

    /// Database is used for user data.
    /// Currently supports mysql or compatible backend.
    pub db_url: Option<String>,

    /// minimum size of the connection pool for the database.
    /// If none, the number of workers are used.
    pub db_min_connections: Option<u32>,

    /// maximum size of the connection pool for the database.
    /// If none, the minimum * 2 is used.
    pub db_max_connections: Option<u32>,

    /// Read-only replica of db_url.
    pub db_replica_url: Option<String>,

    /// minimum size of the connection pool for the database replica.
    /// If none, db_min_connections is used.
    pub db_replica_min_connections: Option<u32>,

    /// maximum size of the connection pool for the database replica.
    /// If none, db_max_connections is used.
    pub db_replica_max_connections: Option<u32>,

    /// Default request limit for registered users.
    /// 0 = block all requests
    /// None = allow all requests
    pub default_user_max_requests_per_period: Option<u64>,

    /// Default ERC address for out deposit contract
    pub deposit_factory_contract: Option<Address>,

    /// Default ERC address for out deposit contract
    pub deposit_topic: Option<H256>,

    /// minimum amount to increase eth_estimateGas results
    pub gas_increase_min: Option<U256>,

    /// percentage to increase eth_estimateGas results. 100 == 100%
    pub gas_increase_percent: Option<U256>,

    /// Restrict user registration.
    /// None = no code needed
    pub invite_code: Option<String>,

    /// Optional kafka brokers
    /// Used by /debug/:rpc_key urls for logging requests and responses. No other endpoints log request/response data.
    pub kafka_urls: Option<String>,

    #[serde(default = "default_kafka_protocol")]
    pub kafka_protocol: String,

    /// domain in sign-in-with-ethereum messages
    pub login_domain: Option<String>,

    /// do not serve any requests if the best known block is older than this many seconds.
    pub max_block_age: Option<u64>,

    /// do not serve any requests if the best known block is behind the best known block by more than this many blocks.
    pub max_block_lag: Option<U64>,

    /// Rate limit for bearer token authenticated entrypoints.
    /// This is separate from the rpc limits.
    #[serde(default = "default_bearer_token_max_concurrent_requests")]
    pub bearer_token_max_concurrent_requests: u64,

    /// Rate limit for the login entrypoint.
    /// This is separate from the rpc limits.
    #[serde(default = "default_login_rate_limit_per_period")]
    pub login_rate_limit_per_period: u64,

    /// The soft limit prevents thundering herds as new blocks are seen.
    #[serde(default = "default_min_sum_soft_limit")]
    pub min_sum_soft_limit: u32,

    /// Another knob for preventing thundering herds as new blocks are seen.
    #[serde(default = "default_min_synced_rpcs")]
    pub min_synced_rpcs: usize,

    /// Concurrent request limit for anonymous users.
    /// Some(0) = block all requests
    /// None = allow all requests
    pub public_max_concurrent_requests: Option<usize>,

    /// Request limit for anonymous users.
    /// Some(0) = block all requests
    /// None = allow all requests
    pub public_requests_per_period: Option<u64>,

    /// Salt for hashing recent ips. Not a perfect way to introduce privacy, but better than nothing
    pub public_recent_ips_salt: Option<String>,

    /// RPC responses are cached locally
    #[serde(default = "default_response_cache_max_bytes")]
    pub response_cache_max_bytes: u64,

    /// the stats page url for an anonymous user.
    pub redirect_public_url: Option<String>,

    /// the stats page url for a logged in user. if set, must contain "{rpc_key_id}"
    pub redirect_rpc_key_url: Option<String>,

    /// Optionally send errors to <https://sentry.io>
    pub sentry_url: Option<String>,

    /// Track rate limits in a redis (or compatible backend)
    /// It is okay if this data is lost.
    pub volatile_redis_url: Option<String>,

    /// maximum size of the connection pool for the cache
    /// If none, the minimum * 2 is used
    pub volatile_redis_max_connections: Option<usize>,

    /// influxdb host for stats
    pub influxdb_host: Option<String>,

    /// influxdb org for stats
    pub influxdb_org: Option<String>,

    /// influxdb token for stats
    pub influxdb_token: Option<String>,

    /// influxdb bucket to use for stats
    pub influxdb_bucket: Option<String>,

    /// unknown config options get put here
    #[serde(flatten, default = "HashMap::default")]
    pub extra: HashMap<String, serde_json::Value>,
}

fn default_archive_depth() -> u64 {
    90_000
}

fn default_allowed_origin_requests_per_period() -> HashMap<String, u64> {
    HashMap::new()
}

/// This might cause a thundering herd!
fn default_min_sum_soft_limit() -> u32 {
    1
}

/// Only require 1 server. This might cause a thundering herd!
fn default_min_synced_rpcs() -> usize {
    1
}

/// Having a low amount of concurrent requests for bearer tokens keeps us from hammering the database.
fn default_bearer_token_max_concurrent_requests() -> u64 {
    2
}

/// Having a low amount of requests per period (usually minute) for login is safest.
fn default_login_rate_limit_per_period() -> u64 {
    10
}

fn default_kafka_protocol() -> String {
    "ssl".to_string()
}

fn default_response_cache_max_bytes() -> u64 {
    // TODO: default to some percentage of the system?
    // 100 megabytes
    10u64.pow(8)
}

/// Configuration for a backend web3 RPC server
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct Web3RpcConfig {
    /// simple way to disable a connection without deleting the row
    #[serde(default)]
    pub disabled: bool,
    /// a name used in /status and other user facing messages
    pub display_name: Option<String>,
    /// (deprecated) rpc url
    pub url: Option<String>,
    /// while not absolutely required, a ws:// or wss:// connection will be able to subscribe to head blocks
    pub ws_url: Option<String>,
    /// while not absolutely required, a http:// or https:// connection will allow erigon to stream JSON
    pub http_url: Option<String>,
    /// block data limit. If None, will be queried
    pub block_data_limit: Option<u64>,
    /// the requests per second at which the server starts slowing down
    pub soft_limit: u32,
    /// the requests per second at which the server throws errors (rate limit or otherwise)
    pub hard_limit: Option<u64>,
    /// only use this rpc if everything else is lagging too far. this allows us to ignore fast but very low limit rpcs
    #[serde(default)]
    pub backup: bool,
    /// Subscribe to the firehose of pending transactions
    /// Don't do this with free rpcs
    #[serde(default)]
    pub subscribe_txs: bool,
    /// unknown config options get put here
    #[serde(flatten, default = "HashMap::default")]
    pub extra: HashMap<String, serde_json::Value>,
}

impl Web3RpcConfig {
    /// Create a Web3Rpc from config
    /// TODO: move this into Web3Rpc? (just need to make things pub(crate))
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn(
        self,
        name: String,
        db_conn: Option<DatabaseConnection>,
        redis_pool: Option<redis_rate_limiter::RedisPool>,
        chain_id: u64,
        http_client: Option<reqwest::Client>,
        blocks_by_hash_cache: BlocksByHashCache,
        block_sender: Option<flume::Sender<BlockAndRpc>>,
        tx_id_sender: Option<flume::Sender<TxHashAndRpc>>,
    ) -> anyhow::Result<(Arc<Web3Rpc>, Web3ProxyJoinHandle<()>)> {
        if !self.extra.is_empty() {
            warn!("unknown Web3RpcConfig fields!: {:?}", self.extra.keys());
        }

        // TODO: get this from config? a helper function? where does this belong?
        let block_interval = match chain_id {
            // ethereum
            1 => Duration::from_secs(12),
            // ethereum-goerli
            5 => Duration::from_secs(12),
            // binance
            56 => Duration::from_secs(3),
            // polygon
            137 => Duration::from_secs(2),
            // fantom
            250 => Duration::from_secs(1),
            // arbitrum
            42161 => Duration::from_millis(500),
            // anything else
            _ => {
                let default = 10;
                warn!(
                    "unknown chain_id ({}). defaulting polling every {} seconds",
                    chain_id, default
                );
                Duration::from_secs(default)
            }
        };

        Web3Rpc::spawn(
            self,
            name,
            chain_id,
            db_conn,
            http_client,
            redis_pool,
            block_interval,
            blocks_by_hash_cache,
            block_sender,
            tx_id_sender,
        )
        .await
    }
}
