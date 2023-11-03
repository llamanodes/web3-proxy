use crate::app::Web3ProxyJoinHandle;
use crate::compute_units::default_usd_per_cu;
use crate::rpcs::blockchain::{BlocksByHashCache, Web3ProxyBlock};
use crate::rpcs::one::Web3Rpc;
use argh::FromArgs;
use deduped_broadcast::DedupedBroadcaster;
use ethers::prelude::{Address, TxHash};
use ethers::types::{U256, U64};
use hashbrown::HashMap;
use migration::sea_orm::prelude::Decimal;
use sentry::types::Dsn;
use serde::{de, Deserialize, Deserializer};
use serde_inline_default::serde_inline_default;
use std::fmt;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::warn;

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
    #[serde(default = "Default::default")]
    pub private_rpcs: HashMap<String, Web3RpcConfig>,
    #[serde(default = "Default::default")]
    pub bundler_4337_rpcs: HashMap<String, Web3RpcConfig>,
    /// unknown config options get put here
    #[serde(flatten, default = "HashMap::default")]
    pub extra: HashMap<String, serde_json::Value>,
}

impl TopConfig {
    /// TODO: this should probably be part of Deserialize
    pub fn clean(&mut self) {
        if !self.extra.is_empty() {
            warn!(
                extra=?self.extra.keys(),
                "unknown TopConfig fields!",
            );
        }

        self.app.clean();
    }
}

/// shared configuration between Web3Rpcs
// TODO: no String, only &str
#[serde_inline_default]
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    /// Request limit for allowed origins for anonymous users.
    /// These requests get rate limited by IP.
    #[serde(default = "Default::default")]
    pub allowed_origin_requests_per_period: HashMap<String, u64>,

    /// erigon defaults to pruning beyond 90,000 blocks
    #[serde_inline_default(90_000u64)]
    pub archive_depth: u64,

    /// pool of extra connections allowed for authenticated users
    #[serde_inline_default(0usize)]
    pub bonus_premium_concurrency: usize,
    /// pool of extra connections allowed for anonymous users
    #[serde_inline_default(0usize)]
    pub bonus_public_concurrency: usize,

    /// pool of extra requests per second allowed for authenticaed users
    #[serde_inline_default(0u64)]
    pub bonus_frontend_public_rate_limit: u64,

    #[serde_inline_default(0u64)]
    pub bonus_frontend_premium_rate_limit: u64,

    /// EVM chain id. 1 for ETH
    /// TODO: better type for chain_id? max of `u64::MAX / 2 - 36` <https://github.com/ethereum/EIPs/issues/2294>
    #[serde_inline_default(1u64)]
    pub chain_id: u64,

    /// Cost per computational unit
    // pub cost_per_cu: Decimal,

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

    /// True if anonymous users should be able to eth_subscribe
    /// newHeads is always allowed because that is cheap to send
    #[serde_inline_default(false)]
    pub free_subscriptions: bool,

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

    #[serde_inline_default("ssl".to_string())]
    pub kafka_protocol: String,

    /// domain in sign-in-with-ethereum messages
    pub login_domain: Option<String>,

    /// do not serve any requests if the best known block is behind the best known block by more than this many blocks.
    pub max_head_block_lag: Option<U64>,

    /// Rate limit for the login entrypoint.
    /// This is separate from the rpc limits.
    #[serde_inline_default(10u64)]
    pub login_rate_limit_per_period: u64,

    /// The soft limit prevents thundering herds as new blocks are seen.
    #[serde_inline_default(1u32)]
    pub min_sum_soft_limit: u32,

    /// Another knob for preventing thundering herds as new blocks are seen.
    #[serde_inline_default(1usize)]
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
    #[serde_inline_default(10u64.pow(8))]
    pub response_cache_max_bytes: u64,

    /// the stats page url for an anonymous user.
    pub redirect_public_url: Option<String>,

    /// the stats page url for a logged in user. if set, must contain "{rpc_key_id}"
    pub redirect_rpc_key_url: Option<String>,

    /// optional script to run before shutting the frontend down.
    /// this is useful for keeping load balancers happy.
    pub shutdown_script: Option<String>,

    /// optional arguments for your shutdown script.
    #[serde_inline_default(vec![])]
    pub shutdown_script_args: Vec<String>,

    /// optional script to run before shutting the frontend down.
    /// this is useful for keeping load balancers happy.
    pub start_script: Option<String>,

    /// optional arguments for your shutdown script.
    #[serde_inline_default(vec![])]
    pub start_script_args: Vec<String>,

    /// Optionally send errors to <https://sentry.io>
    pub sentry_url: Option<Dsn>,

    /// Stripe api key for checking validity of webhooks
    pub stripe_whsec_key: Option<String>,

    pub usd_per_cu: Option<Decimal>,

    /// Track rate limits in a redis (or compatible backend)
    /// It is okay if this data is lost.
    pub volatile_redis_url: Option<String>,

    /// maximum size of the connection pool for the cache
    /// If none, workers * 2 is used
    pub volatile_redis_max_connections: Option<usize>,

    /// influxdb host for stats
    pub influxdb_host: Option<String>,

    /// influxdb org for stats
    pub influxdb_org: Option<String>,

    /// influxdb token for stats
    pub influxdb_token: Option<String>,

    /// influxdb bucket to use for stats
    pub influxdb_bucket: Option<String>,

    /// unique_id keeps stats from different servers being seen as duplicates of each other.
    /// this int is used as part of the "nanoseconds" part of the influx timestamp.
    /// it can also be used by the rate limiter.
    ///
    /// This **MUST** be set to a unique value for each running server.
    /// If not set, severs will overwrite eachother's stats!
    ///
    /// <https://docs.influxdata.com/influxdb/v2.0/write-data/best-practices/duplicate-points/#increment-the-timestamp>
    #[serde_inline_default(0i64)]
    pub unique_id: i64,

    /// unknown config options get put here
    #[serde(flatten, default = "HashMap::default")]
    pub extra: HashMap<String, serde_json::Value>,
}

impl Default for AppConfig {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

impl AppConfig {
    /// TODO: this should probably be part of Deserialize
    fn clean(&mut self) {
        if self.usd_per_cu.is_none() {
            self.usd_per_cu = Some(default_usd_per_cu(self.chain_id));
        }

        if let Some(influxdb_id) = self.extra.get("influxdb_id") {
            self.unique_id = influxdb_id.as_i64().unwrap();
        }

        if !self.extra.is_empty() {
            warn!(
                extra=?self.extra.keys(),
                "unknown Web3ProxyAppConfig fields!",
            );
        }
    }
}

/// TODO: we can't query a provider because we need this to create a provider
/// TODO: cache this
pub fn average_block_interval(chain_id: u64) -> Duration {
    match chain_id {
        // ethereum
        1 => Duration::from_secs(12),
        // ethereum-goerli
        5 => Duration::from_secs(12),
        // optimism
        10 => Duration::from_secs(2),
        // binance
        56 => Duration::from_secs(3),
        // polygon
        137 => Duration::from_secs(2),
        // fantom
        250 => Duration::from_secs(1),
        // zkevm polygon
        1101 => Duration::from_secs(7),
        // base
        8453 => Duration::from_secs(2),
        // development
        31337 => Duration::from_secs(10),
        // arbitrum
        42161 => Duration::from_millis(500),
        // web3-proxy tests
        999_001_999 => Duration::from_secs(10),
        // anything else
        _ => {
            let default = 10;
            warn!(
                "unknown chain_id ({}). defaulting average_block_interval to {} seconds",
                chain_id, default
            );
            Duration::from_secs(default)
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum BlockDataLimit {
    /// archive nodes can return all data
    Archive,
    /// prune nodes don't have all the data
    /// some devs will argue about what "prune" means but we use it to mean that any of the data is gone.
    /// TODO: this is too simple. erigon can prune the different types of data differently
    Set(u64),
    /// Automatically detect the limit
    #[default]
    Unknown,
}

impl<'de> Deserialize<'de> for BlockDataLimit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BlockDataLimitVisitor;

        impl<'de> de::Visitor<'de> for BlockDataLimitVisitor {
            type Value = BlockDataLimit;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a string 'archive', 'unknown' or an positive signed 64-bit integer. 0 means automatically detect")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                match value.to_ascii_lowercase().as_str() {
                    "archive" => Ok(BlockDataLimit::Archive),
                    "unknown" => Ok(BlockDataLimit::Unknown),
                    _ => Err(de::Error::custom(format!("Unexpected value {}", value))),
                }
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                if v < 0 {
                    Err(de::Error::custom("Negative values are not allowed"))
                } else {
                    Ok(BlockDataLimit::Set(v as u64))
                }
            }
        }

        deserializer.deserialize_any(BlockDataLimitVisitor)
    }
}

impl From<BlockDataLimit> for AtomicU64 {
    fn from(value: BlockDataLimit) -> Self {
        match value {
            BlockDataLimit::Archive => AtomicU64::new(u64::MAX),
            BlockDataLimit::Set(limit) => AtomicU64::new(limit),
            BlockDataLimit::Unknown => AtomicU64::new(0),
        }
    }
}

/// Configuration for a backend web3 RPC server
#[serde_inline_default]
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Web3RpcConfig {
    /// simple way to disable a connection without deleting the row
    #[serde(default = "Default::default")]
    pub disabled: bool,
    /// a name used in /status and other user facing messages
    pub display_name: Option<String>,
    /// while not absolutely required, a ws:// or wss:// connection will be able to subscribe to head blocks
    pub ws_url: Option<String>,
    /// while not absolutely required, a http:// or https:// connection will allow erigon to stream JSON
    pub http_url: Option<String>,
    /// block data limit. If None, will be queried
    #[serde(default = "Default::default")]
    pub block_data_limit: BlockDataLimit,
    /// the requests per second at which the server starts slowing down
    #[serde_inline_default(1u32)]
    pub soft_limit: u32,
    /// the requests per period at which the server throws errors (rate limit or otherwise)
    pub hard_limit: Option<u64>,
    /// the number of seconds in a rate limiting period
    /// some providers allow burst limits and rolling windows, but coding that is a lot more complicated
    #[serde_inline_default(1u32)]
    pub hard_limit_period: u32,
    /// if hard limits are applied per server or per endpoint. default is per server
    #[serde(default = "Default::default")]
    pub hard_limit_per_endpoint: bool,
    /// only use this rpc if everything else is lagging too far. this allows us to ignore fast but very low limit rpcs
    #[serde(default = "Default::default")]
    pub backup: bool,
    /// Subscribe to the firehose of pending transactions
    /// Don't do this with free rpcs
    #[serde(default = "Default::default")]
    pub subscribe_txs: bool,
    /// unknown config options get put here
    #[serde(flatten, default = "HashMap::default")]
    pub extra: HashMap<String, serde_json::Value>,
}

impl Default for Web3RpcConfig {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

impl Web3RpcConfig {
    /// Create a Web3Rpc from config
    /// TODO: move this into Web3Rpc? (just need to make things pub(crate))
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn(
        self,
        name: String,
        redis_pool: Option<redis_rate_limiter::RedisPool>,
        server_id: i64,
        chain_id: u64,
        block_interval: Duration,
        http_client: Option<reqwest::Client>,
        blocks_by_hash_cache: BlocksByHashCache,
        block_and_rpc_sender: Option<mpsc::UnboundedSender<BlockAndRpc>>,
        pending_txid_firehouse: Option<Arc<DedupedBroadcaster<TxHash>>>,
        max_head_block_age: Duration,
    ) -> anyhow::Result<(Arc<Web3Rpc>, Web3ProxyJoinHandle<()>)> {
        if !self.extra.is_empty() {
            // TODO: move this to a `clean` function
            warn!(extra=?self.extra.keys(), "unknown Web3RpcConfig fields!");
        }

        Web3Rpc::spawn(
            self,
            name,
            chain_id,
            http_client,
            redis_pool,
            server_id,
            block_interval,
            blocks_by_hash_cache,
            block_and_rpc_sender,
            pending_txid_firehouse,
            max_head_block_age,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::{AppConfig, Web3RpcConfig};
    use serde_json::json;

    #[test]
    fn expected_app_defaults() {
        // a is from serde
        let a: AppConfig = serde_json::from_value(json!({
            "chain_id": 1,
        }))
        .unwrap();

        assert_eq!(a.min_synced_rpcs, 1);

        // b is from Default
        let b = AppConfig::default();

        assert_eq!(b.min_synced_rpcs, 1);

        assert_eq!(a, b);
    }

    #[test]
    fn expected_rpc_defaults() {
        let a: Web3RpcConfig = serde_json::from_str("{}").unwrap();

        assert_eq!(a.soft_limit, 1);

        let b: Web3RpcConfig = Default::default();

        assert_eq!(b.soft_limit, 1);

        assert_eq!(a, b);
    }
}
