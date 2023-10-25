mod ws;

use crate::caches::{RegisteredUserRateLimitKey, RpcSecretKeyCache, UserBalanceCache};
use crate::config::{AppConfig, TopConfig};
use crate::errors::{Web3ProxyError, Web3ProxyErrorContext, Web3ProxyResult};
use crate::frontend::authorization::Authorization;
use crate::globals::{global_db_conn, DatabaseError, APP, DB_CONN, DB_REPLICA};
use crate::jsonrpc::{
    self, JsonRpcErrorData, JsonRpcParams, JsonRpcRequestEnum, JsonRpcResultData, LooseId,
    ParsedResponse, SingleRequest, SingleResponse, ValidatedRequest,
};
use crate::relational_db::{connect_db, migrate_db};
use crate::response_cache::{ForwardedResponse, JsonRpcResponseCache, JsonRpcResponseWeigher};
use crate::rpcs::blockchain::Web3ProxyBlock;
use crate::rpcs::consensus::RankedRpcs;
use crate::rpcs::many::Web3Rpcs;
use crate::rpcs::one::Web3Rpc;
use crate::rpcs::provider::{connect_http, EthersHttpProvider};
use crate::stats::{AppStat, FlushedStats, StatBuffer};
use anyhow::Context;
use axum::http::StatusCode;
use chrono::Utc;
use deduped_broadcast::DedupedBroadcaster;
use deferred_rate_limiter::DeferredRateLimiter;
use entities::user;
use ethers::core::utils::keccak256;
use ethers::prelude::{Address, Bytes, Transaction, TxHash, H256, U256, U64};
use ethers::utils::rlp::{Decodable, Rlp};
use futures::future::join_all;
use futures::stream::FuturesUnordered;
use hashbrown::{HashMap, HashSet};
use migration::sea_orm::{EntityTrait, PaginatorTrait};
use moka::future::{Cache, CacheBuilder};
use once_cell::sync::OnceCell;
use redis_rate_limiter::redis::AsyncCommands;
use redis_rate_limiter::{redis, DeadpoolRuntime, RedisConfig, RedisPool, RedisRateLimiter};
use serde::Serialize;
use serde_json::json;
use serde_json::value::RawValue;
use std::fmt;
use std::net::IpAddr;
use std::num::NonZeroU64;
use std::str::FromStr;
use std::sync::atomic::{self, AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot, watch, Semaphore};
use tokio::task::{yield_now, JoinHandle};
use tokio::time::{sleep, sleep_until, timeout_at, Instant};
use tokio::{pin, select};
use tracing::{error, info, trace, warn};

// TODO: make this customizable?
// TODO: include GIT_REF in here. i had trouble getting https://docs.rs/vergen/latest/vergen/ to work with a workspace. also .git is in .dockerignore
pub static APP_USER_AGENT: &str = concat!(
    "llamanodes_",
    env!("CARGO_PKG_NAME"),
    "/v",
    env!("CARGO_PKG_VERSION")
);

/// aggregate across 1 week
pub const BILLING_PERIOD_SECONDS: i64 = 60 * 60 * 24 * 7;

/// Convenience type
pub type Web3ProxyJoinHandle<T> = JoinHandle<Web3ProxyResult<T>>;

/// The application
// TODO: i'm sure this is more arcs than necessary, but spawning futures makes references hard
pub struct App {
    /// Send requests to the best server available
    pub balanced_rpcs: Arc<Web3Rpcs>,
    /// Send 4337 Abstraction Bundler requests to one of these servers
    pub bundler_4337_rpcs: Arc<Web3Rpcs>,
    /// application config
    /// TODO: this will need a large refactor to handle reloads while running. maybe use a watch::Receiver and a task_local?
    pub config: AppConfig,
    pub http_client: Option<reqwest::Client>,
    /// track JSONRPC responses
    pub jsonrpc_response_cache: JsonRpcResponseCache,
    /// track JSONRPC cache keys that have failed caching
    pub jsonrpc_response_failed_cache_keys: Cache<u64, ()>,
    /// de-dupe requests (but with easy timeouts)
    pub jsonrpc_response_semaphores: Cache<u64, Arc<Semaphore>>,
    /// rpc clients that subscribe to newHeads use this channel
    /// don't drop this or the sender will stop working
    /// TODO: broadcast channel instead?
    pub watch_consensus_head_receiver: watch::Receiver<Option<Web3ProxyBlock>>,
    /// rpc clients that subscribe to newPendingTransactions use this channel
    pub pending_txid_firehose: Arc<DedupedBroadcaster<TxHash>>,
    pub hostname: Option<String>,
    pub frontend_port: Arc<AtomicU16>,
    /// rate limit anonymous users
    pub frontend_public_rate_limiter: Option<DeferredRateLimiter<IpAddr>>,
    /// bonus rate limit for anonymous users
    pub bonus_frontend_public_rate_limiter: Option<RedisRateLimiter>,
    /// rate limit authenticated users
    pub frontend_premium_rate_limiter: Option<DeferredRateLimiter<RegisteredUserRateLimitKey>>,
    /// bonus rate limit for authenticated users
    pub bonus_frontend_premium_rate_limiter: Option<RedisRateLimiter>,
    /// concurrent/parallel request limits for anonymous users
    pub ip_semaphores: Cache<IpAddr, Arc<Semaphore>>,
    /// give some bonus capacity to public users
    pub bonus_ip_concurrency: Arc<Semaphore>,
    /// the /debug/ rpc endpoints send detailed logging to kafka
    #[cfg(feature = "rdkafka")]
    pub kafka_producer: Option<rdkafka::producer::FutureProducer>,
    /// rate limit the login endpoint
    /// we do this because each pending login is a row in the database
    pub login_rate_limiter: Option<RedisRateLimiter>,
    /// Send private requests (like eth_sendRawTransaction) to all these servers
    pub protected_rpcs: Arc<Web3Rpcs>,
    pub prometheus_port: Arc<AtomicU16>,
    /// cache authenticated users so that we don't have to query the database on the hot path
    // TODO: should the key be our RpcSecretKey class instead of Ulid?
    pub rpc_secret_key_cache: RpcSecretKeyCache,
    /// cache user balances so we don't have to check downgrade logic every single time
    pub user_balance_cache: UserBalanceCache,
    /// concurrent/parallel RPC request limits for authenticated users
    pub user_semaphores: Cache<(NonZeroU64, IpAddr), Arc<Semaphore>>,
    /// give some bonus capacity to premium users
    pub bonus_user_concurrency: Arc<Semaphore>,
    /// volatile cache used for rate limits
    /// TODO: i think i might just delete this entirely. instead use local-only concurrency limits.
    pub vredis_pool: Option<RedisPool>,
    /// channel for sending stats in a background task
    pub stat_sender: Option<mpsc::UnboundedSender<AppStat>>,
    /// when the app started
    pub start: Instant,

    /// Optional time series database for making pretty graphs that load quickly
    influxdb_client: Option<influxdb2::Client>,
    /// Simple way to connect ethers Contracsts to the proxy
    /// TODO: make this more efficient
    internal_provider: OnceCell<Arc<EthersHttpProvider>>,
}

/// starting an app creates many tasks
pub struct Web3ProxyAppSpawn {
    /// the app. probably clone this to use in other groups of handles
    pub app: Arc<App>,
    /// handle for some rpcs
    pub balanced_handle: Web3ProxyJoinHandle<()>,
    /// handle for some rpcs
    pub private_handle: Web3ProxyJoinHandle<()>,
    /// handle for some rpcs
    pub bundler_4337_rpcs_handle: Web3ProxyJoinHandle<()>,
    /// these are important and must be allowed to finish
    pub background_handles: FuturesUnordered<Web3ProxyJoinHandle<()>>,
    /// config changes are sent here
    pub new_top_config: Arc<watch::Sender<TopConfig>>,
    /// watch this to know when the app is ready to serve requests
    pub ranked_rpcs: watch::Receiver<Option<Arc<RankedRpcs>>>,
}

impl App {
    /// The main entrypoint.
    pub async fn spawn(
        frontend_port: Arc<AtomicU16>,
        prometheus_port: Arc<AtomicU16>,
        mut top_config: TopConfig,
        num_workers: usize,
        shutdown_sender: broadcast::Sender<()>,
        flush_stat_buffer_sender: mpsc::Sender<oneshot::Sender<FlushedStats>>,
        flush_stat_buffer_receiver: mpsc::Receiver<oneshot::Sender<FlushedStats>>,
    ) -> anyhow::Result<Web3ProxyAppSpawn> {
        let stat_buffer_shutdown_receiver = shutdown_sender.subscribe();
        let mut config_watcher_shutdown_receiver = shutdown_sender.subscribe();
        let mut background_shutdown_receiver = shutdown_sender.subscribe();

        top_config.clean();

        let (new_top_config_sender, mut new_top_config_receiver) =
            watch::channel(top_config.clone());
        new_top_config_receiver.borrow_and_update();

        // TODO: take this from config
        // TODO: how should we handle hitting this max?
        let max_users = 20_000;

        // safety checks on the config
        // while i would prefer this to be in a "apply_top_config" function, that is a larger refactor
        // TODO: maybe don't spawn with a config at all. have all config updates come through an apply_top_config call
        if let Some(redirect) = &top_config.app.redirect_rpc_key_url {
            assert!(
                redirect.contains("{{rpc_key_id}}"),
                "redirect_rpc_key_url user url must contain \"{{rpc_key_id}}\""
            );
        }

        // we must wait for these to end on their own (and they need to subscribe to shutdown_sender)
        // TODO: is FuturesUnordered what we need? I want to return when the first one returns
        let important_background_handles: FuturesUnordered<Web3ProxyJoinHandle<()>> =
            FuturesUnordered::new();

        // connect to kafka for logging requests from the /debug/ urls

        #[cfg(feature = "rdkafka")]
        let mut kafka_producer: Option<rdkafka::producer::FutureProducer> = None;

        #[cfg(feature = "rdkafka")]
        if let Some(kafka_brokers) = top_config.app.kafka_urls.clone() {
            info!("Connecting to kafka");

            let security_protocol = &top_config.app.kafka_protocol;

            match rdkafka::ClientConfig::new()
                .set("bootstrap.servers", kafka_brokers)
                .set("message.timeout.ms", "5000")
                .set("security.protocol", security_protocol)
                .create()
            {
                Ok(k) => {
                    // TODO: create our topic
                    kafka_producer = Some(k)
                }
                Err(err) => error!("Failed connecting to kafka. This will not retry. {:?}", err),
            }
        }

        // TODO: do this during apply_config so that we can change redis url while running
        // create a connection pool for redis
        // a failure to connect does NOT block the application from starting
        let vredis_pool = match top_config.app.volatile_redis_url.as_ref() {
            Some(redis_url) => {
                // TODO: scrub credentials and then include the redis_url in logs
                info!("Connecting to vredis");

                // TODO: what is a good default?
                let redis_max_connections = top_config
                    .app
                    .volatile_redis_max_connections
                    .unwrap_or(num_workers * 2);

                // TODO: what are reasonable timeouts?
                let redis_pool = RedisConfig::from_url(redis_url)
                    .builder()?
                    .max_size(redis_max_connections)
                    .runtime(DeadpoolRuntime::Tokio1)
                    .build()?;

                // test the redis pool
                if let Err(err) = redis_pool.get().await {
                    error!(
                        "failed to connect to vredis. some features will be disabled. err={:?}",
                        err
                    );
                };

                Some(redis_pool)
            }
            None => {
                warn!("no redis connection. some features will be disabled");
                None
            }
        };

        let influxdb_client = match top_config.app.influxdb_host.as_ref() {
            Some(influxdb_host) => {
                let influxdb_org = top_config
                    .app
                    .influxdb_org
                    .clone()
                    .expect("influxdb_org needed when influxdb_host is set");

                let influxdb_token = top_config
                    .app
                    .influxdb_token
                    .clone()
                    .expect("influxdb_token needed when influxdb_host is set");

                top_config
                    .app
                    .influxdb_bucket
                    .as_ref()
                    .expect("influxdb_bucket needed when influxdb_host is set");

                let influxdb_client =
                    influxdb2::Client::new(influxdb_host, influxdb_org, influxdb_token);

                // TODO: test the client now. having a stat for "started" can be useful on graphs to mark deploys

                Some(influxdb_client)
            }
            None => None,
        };

        // all the users are the same size, so no need for a weigher
        // if there is no database of users, there will be no keys and so this will be empty
        // TODO: max_capacity from config
        // TODO: ttl from config
        let rpc_secret_key_cache = CacheBuilder::new(max_users)
            .name("rpc_secret_key")
            .time_to_live(Duration::from_secs(600))
            .build();

        // TODO: TTL left low, this could also be a solution instead of modifiying the cache, that may be disgusting across threads / slow anyways
        let user_balance_cache: UserBalanceCache = CacheBuilder::new(max_users)
            .name("user_balance")
            .time_to_live(Duration::from_secs(600))
            .build()
            .into();

        // create a channel for receiving stats
        // we do this in a channel so we don't slow down our response to the users
        // stats can be saved in mysql, influxdb, both, or none
        let stat_sender = if let Some(spawned_stat_buffer) = StatBuffer::try_spawn(
            BILLING_PERIOD_SECONDS,
            top_config.app.chain_id,
            30,
            top_config.app.influxdb_bucket.clone(),
            influxdb_client.clone(),
            rpc_secret_key_cache.clone(),
            user_balance_cache.clone(),
            stat_buffer_shutdown_receiver,
            10,
            flush_stat_buffer_sender.clone(),
            flush_stat_buffer_receiver,
            top_config.app.unique_id,
        )? {
            // since the database entries are used for accounting, we want to be sure everything is saved before exiting
            important_background_handles.push(spawned_stat_buffer.background_handle);

            Some(spawned_stat_buffer.stat_sender)
        } else {
            info!("stats will not be collected");
            None
        };

        // make a http shared client
        // TODO: can we configure the connection pool? should we?
        // TODO: timeouts from config. defaults are hopefully good
        // TODO: is always disabling compression a good idea?
        let http_client = Some(
            reqwest::ClientBuilder::new()
                .connect_timeout(Duration::from_secs(5))
                .no_brotli()
                .no_deflate()
                .no_gzip()
                .timeout(Duration::from_secs(5 * 60 - 2))
                .user_agent(APP_USER_AGENT)
                .build()?,
        );

        // create rate limiters
        // these are optional. they require redis
        let mut frontend_public_rate_limiter = None;
        let mut frontend_premium_rate_limiter = None;
        let mut login_rate_limiter = None;
        let mut bonus_frontend_public_rate_limiter: Option<RedisRateLimiter> = None;
        let mut bonus_frontend_premium_rate_limiter: Option<RedisRateLimiter> = None;

        if let Some(ref redis_pool) = vredis_pool {
            if let Some(public_requests_per_period) = top_config.app.public_requests_per_period {
                // chain id is included in the app name so that rpc rate limits are per-chain
                let rpc_rrl = RedisRateLimiter::new(
                    &format!("web3_proxy:{}", top_config.app.chain_id),
                    "frontend",
                    public_requests_per_period,
                    60.0,
                    redis_pool.clone(),
                );

                // these two rate limiters can share the base limiter
                // these are deferred rate limiters because we don't want redis network requests on the hot path
                // TODO: take cache_size from config
                frontend_public_rate_limiter =
                    Some(DeferredRateLimiter::new(20_000, "ip", rpc_rrl.clone(), None).await);
                frontend_premium_rate_limiter =
                    Some(DeferredRateLimiter::new(20_000, "key", rpc_rrl, None).await);

                if top_config.app.bonus_frontend_public_rate_limit > 0 {
                    bonus_frontend_public_rate_limiter = Some(RedisRateLimiter::new(
                        "web3_proxy",
                        "bonus_frontend_public",
                        top_config.app.bonus_frontend_public_rate_limit,
                        60.0,
                        redis_pool.clone(),
                    ));
                }
                if top_config.app.bonus_frontend_premium_rate_limit > 0 {
                    bonus_frontend_premium_rate_limiter = Some(RedisRateLimiter::new(
                        "web3_proxy",
                        "bonus_frontend_premium",
                        top_config.app.bonus_frontend_premium_rate_limit,
                        60.0,
                        redis_pool.clone(),
                    ));
                }
            }

            // login rate limiter
            login_rate_limiter = Some(RedisRateLimiter::new(
                "web3_proxy",
                "login",
                top_config.app.login_rate_limit_per_period,
                60.0,
                redis_pool.clone(),
            ));
        }

        let (watch_consensus_head_sender, watch_consensus_head_receiver) = watch::channel(None);

        // responses can be very different in sizes, so this is a cache with a max capacity and a weigher
        // TODO: we should emit stats to calculate a more accurate expected cache size
        // TODO: do we actually want a TTL on this?
        // TODO: configurable max item weight insted of hard coding to .1% of the cache?
        let jsonrpc_weigher =
            JsonRpcResponseWeigher((top_config.app.response_cache_max_bytes / 1000) as u32);

        let jsonrpc_response_cache: JsonRpcResponseCache =
            CacheBuilder::new(top_config.app.response_cache_max_bytes)
                .name("jsonrpc_response_cache")
                .time_to_idle(Duration::from_secs(3600))
                .weigher(move |k, v| jsonrpc_weigher.weigh(k, v))
                .build();

        // create semaphores for concurrent connection limits
        // TODO: time-to-idle on these. need to make sure the arcs aren't anywhere though. so maybe arc isn't correct and it should be refs
        let ip_semaphores = CacheBuilder::new(max_users).name("ip_semaphores").build();
        let user_semaphores = CacheBuilder::new(max_users).name("user_semaphores").build();

        let chain_id = top_config.app.chain_id;

        // TODO: deduped_txid_firehose capacity from config
        let deduped_txid_firehose = DedupedBroadcaster::new(100, 20_000);

        // TODO: remove this. it should only be done by apply_top_config
        let (balanced_rpcs, balanced_handle, consensus_connections_watcher) = Web3Rpcs::spawn(
            chain_id,
            top_config.app.max_head_block_lag,
            top_config.app.min_synced_rpcs,
            top_config.app.min_sum_soft_limit,
            "balanced rpcs".into(),
            Some(watch_consensus_head_sender),
            Some(deduped_txid_firehose.clone()),
        )
        .await
        .web3_context("spawning balanced rpcs")?;

        // prepare a Web3Rpcs to hold all our private connections
        // only some chains have this, so this might be empty
        // TODO: set min_sum_soft_limit > 0 if any private rpcs are configured. this way we don't accidently leak to the public mempool if they are all offline
        let (private_rpcs, private_handle, _) = Web3Rpcs::spawn(
            chain_id,
            // private rpcs don't get subscriptions, so no need for max_head_block_lag
            None,
            0,
            0,
            "protected rpcs".into(),
            // subscribing to new heads here won't work well. if they are fast, they might be ahead of balanced_rpcs
            // they also often have low rate limits
            // however, they are well connected to miners/validators. so maybe using them as a safety check would be good
            // TODO: but maybe we could include privates in the "backup" tier
            None,
            None,
        )
        .await
        .web3_context("spawning private_rpcs")?;

        // prepare a Web3Rpcs to hold all our 4337 Abstraction Bundler connections (if any)
        let (bundler_4337_rpcs, bundler_4337_rpcs_handle, _) = Web3Rpcs::spawn(
            chain_id,
            // bundler_4337_rpcs don't get subscriptions, so no need for max_head_block_lag
            None,
            0,
            0,
            "eip4337 rpcs".into(),
            None,
            None,
        )
        .await
        .web3_context("spawning bundler_4337_rpcs")?;

        let hostname = hostname::get()
            .ok()
            .and_then(|x| x.to_str().map(|x| x.to_string()));

        let bonus_ip_concurrency =
            Arc::new(Semaphore::new(top_config.app.bonus_public_concurrency));
        let bonus_user_concurrency =
            Arc::new(Semaphore::new(top_config.app.bonus_premium_concurrency));

        // TODO: what size?
        let jsonrpc_response_semaphores = CacheBuilder::new(10_000)
            .name("jsonrpc_response_semaphores")
            .build();

        let jsonrpc_response_failed_cache_keys = CacheBuilder::new(100_000)
            .name("jsonrpc_response_failed_cache_keys")
            .build();

        let app = Self {
            balanced_rpcs,
            bonus_frontend_public_rate_limiter,
            bonus_frontend_premium_rate_limiter,
            bonus_ip_concurrency,
            bonus_user_concurrency,
            bundler_4337_rpcs,
            config: top_config.app.clone(),
            frontend_public_rate_limiter,
            frontend_port: frontend_port.clone(),
            frontend_premium_rate_limiter,
            hostname,
            http_client,
            influxdb_client,
            internal_provider: Default::default(),
            ip_semaphores,
            jsonrpc_response_cache,
            jsonrpc_response_failed_cache_keys,
            jsonrpc_response_semaphores,
            #[cfg(feature = "rdkafka")]
            kafka_producer,
            login_rate_limiter,
            pending_txid_firehose: deduped_txid_firehose,
            protected_rpcs: private_rpcs,
            prometheus_port: prometheus_port.clone(),
            rpc_secret_key_cache,
            start: Instant::now(),
            stat_sender,
            user_balance_cache,
            user_semaphores,
            vredis_pool,
            watch_consensus_head_receiver,
        };

        let app = Arc::new(app);

        if let Err(app) = APP.set(app.clone()) {
            error!(?app, "global APP can only be set once!");
        };

        // TODO: do apply_top_config once we don't duplicate the db
        if let Err(err) = app.apply_top_config_db(&top_config).await {
            warn!(?err, "unable to fully apply config while starting!");
        };

        // watch for config changes
        // TODO: move this to its own function/struct
        {
            let app = app.clone();
            let config_handle = tokio::spawn(async move {
                loop {
                    let new_top_config = new_top_config_receiver.borrow_and_update().to_owned();

                    // TODO: compare new and old here? the sender should be doing that already but maybe its better here

                    if let Err(err) = app.apply_top_config_rpcs(&new_top_config).await {
                        error!(?err, "unable to apply config! Retrying in 10 seconds (or if the config changes)");

                        select! {
                            _ = config_watcher_shutdown_receiver.recv() => {
                                break;
                            }
                            _ = sleep(Duration::from_secs(10)) => {}
                            _ = new_top_config_receiver.changed() => {}
                        }
                    } else {
                        // configs applied successfully. wait for configs to change or for the app to exit
                        select! {
                            _ = config_watcher_shutdown_receiver.recv() => {
                                break;
                            }
                            _ = new_top_config_receiver.changed() => {}
                        }
                    }

                    // TODO: add a min time between config changes
                    yield_now().await;
                }

                Ok(())
            });

            important_background_handles.push(config_handle);
        }

        if important_background_handles.is_empty() {
            trace!("no important background handles");

            let f = tokio::spawn(async move {
                let _ = background_shutdown_receiver.recv().await;

                Ok(())
            });

            important_background_handles.push(f);
        }

        Ok(Web3ProxyAppSpawn {
            app,
            balanced_handle,
            private_handle,
            bundler_4337_rpcs_handle,
            background_handles: important_background_handles,
            new_top_config: Arc::new(new_top_config_sender),
            ranked_rpcs: consensus_connections_watcher,
        })
    }

    pub async fn apply_top_config(&self, new_top_config: &TopConfig) -> Web3ProxyResult<()> {
        // TODO: update self.config from new_top_config.app (or move it entirely to a global)

        // connect to the db first
        let db = self.apply_top_config_db(new_top_config).await;

        // then refresh rpcs
        let rpcs = self.apply_top_config_rpcs(new_top_config).await;

        // error if either failed
        // TODO: if both error, log both errors
        db?;
        rpcs?;

        Ok(())
    }

    async fn apply_top_config_rpcs(&self, new_top_config: &TopConfig) -> Web3ProxyResult<()> {
        info!("applying new config");

        let balanced = self
            .balanced_rpcs
            .apply_server_configs(self, &new_top_config.balanced_rpcs)
            .await
            .web3_context("updating balanced rpcs");

        let protected = self
            .protected_rpcs
            .apply_server_configs(self, &new_top_config.private_rpcs)
            .await
            .web3_context("updating private_rpcs");

        let bundler_4337 = self
            .bundler_4337_rpcs
            .apply_server_configs(self, &new_top_config.bundler_4337_rpcs)
            .await
            .web3_context("updating bundler_4337_rpcs");

        // TODO: log all the errors if there are multiple
        balanced?;
        protected?;
        bundler_4337?;

        Ok(())
    }

    pub async fn apply_top_config_db(&self, new_top_config: &TopConfig) -> Web3ProxyResult<()> {
        // TODO: get the actual value
        let num_workers = 2;

        // connect to the db
        // THIS DOES NOT RUN MIGRATIONS!
        if let Some(db_url) = new_top_config.app.db_url.clone() {
            let db_min_connections = new_top_config
                .app
                .db_min_connections
                .unwrap_or(num_workers as u32);

            // TODO: what default multiple?
            let db_max_connections = new_top_config
                .app
                .db_max_connections
                .unwrap_or(db_min_connections * 2);

            let db_conn = if let Ok(old_db_conn) = global_db_conn() {
                // TODO: compare old settings with new settings. don't always re-use!
                Ok(old_db_conn)
            } else {
                connect_db(db_url.clone(), db_min_connections, db_max_connections)
                    .await
                    .map_err(|err| DatabaseError::Connect(Arc::new(err)))
            };

            let db_replica = if let Ok(db_conn) = db_conn.as_ref() {
                let db_replica =
                    if let Some(db_replica_url) = new_top_config.app.db_replica_url.clone() {
                        if db_replica_url == db_url {
                            // url is the same. do not make a new connection or we might go past our max connections
                            Ok(db_conn.clone().into())
                        } else {
                            let db_replica_min_connections = new_top_config
                                .app
                                .db_replica_min_connections
                                .unwrap_or(db_min_connections);

                            let db_replica_max_connections = new_top_config
                                .app
                                .db_replica_max_connections
                                .unwrap_or(db_max_connections);

                            let db_replica = if let Ok(old_db_replica) = global_db_conn() {
                                // TODO: compare old settings with new settings. don't always re-use!
                                Ok(old_db_replica)
                            } else {
                                connect_db(
                                    db_replica_url,
                                    db_replica_min_connections,
                                    db_replica_max_connections,
                                )
                                .await
                                .map_err(|err| DatabaseError::Connect(Arc::new(err)))
                            };

                            // if db_replica is error, but db_conn is success. log error and just use db_conn

                            if let Err(err) = db_replica.as_ref() {
                                error!(?err, "db replica is down! using primary");

                                Ok(db_conn.clone().into())
                            } else {
                                db_replica.map(Into::into)
                            }
                        }
                    } else {
                        // just clone so that we don't need a bunch of checks all over our code
                        trace!("no db replica configured");
                        Ok(db_conn.clone().into())
                    };

                // db and replica are connected. try to migrate
                if let Err(err) = migrate_db(db_conn, false).await {
                    error!(?err, "unable to migrate!");
                }

                db_replica
            } else {
                db_conn.clone().map(Into::into)
            };

            let mut locked_conn = DB_CONN.write();
            let mut locked_replica = DB_REPLICA.write();

            *locked_conn = db_conn.clone();
            *locked_replica = db_replica.clone();

            db_conn?;
            db_replica?;

            info!("set global db connections");
        } else if new_top_config.app.db_replica_url.is_some() {
            return Err(anyhow::anyhow!("db_replica_url set, but no db_url set!").into());
        } else {
            warn!("no database. some features will be disabled");
        };

        Ok(())
    }

    pub fn head_block_receiver(&self) -> watch::Receiver<Option<Web3ProxyBlock>> {
        self.watch_consensus_head_receiver.clone()
    }

    pub fn influxdb_client(&self) -> Web3ProxyResult<&influxdb2::Client> {
        self.influxdb_client
            .as_ref()
            .ok_or(Web3ProxyError::NoDatabaseConfigured)
    }

    /// an ethers provider that you can use with ether's abigen.
    /// this works for now, but I don't like it
    /// TODO: I would much prefer we figure out the traits and `impl JsonRpcClient for Web3ProxyApp`
    pub fn internal_provider(&self) -> &Arc<EthersHttpProvider> {
        self.internal_provider.get_or_init(|| {
            // TODO: i'm sure theres much better ways to do this, but i don't want to spend time fighting traits right now
            // TODO: what interval? i don't think we use it
            // i tried and failed to `impl JsonRpcClient for Web3ProxyApi`
            // i tried and failed to set up ipc. http is already running, so lets just use that
            let frontend_port = self.frontend_port.load(Ordering::Relaxed);

            if frontend_port == 0 {
                panic!("frontend is not running. cannot create provider yet");
            }

            let internal_provider = connect_http(
                format!("http://127.0.0.1:{}", frontend_port)
                    .parse()
                    .unwrap(),
                self.http_client.clone(),
                Duration::from_secs(10),
            )
            .unwrap();

            Arc::new(internal_provider)
        })
    }

    pub async fn prometheus_metrics(&self) -> String {
        let globals = HashMap::new();
        // TODO: what globals? should this be the hostname or what?
        // globals.insert("service", "web3_proxy");

        // TODO: this needs a refactor to get HELP and TYPE into the serialized text
        #[derive(Default, Serialize)]
        struct UserCount(i64);

        let user_count: UserCount = if let Ok(db) = global_db_conn() {
            match user::Entity::find().count(&db).await {
                Ok(user_count) => UserCount(user_count as i64),
                Err(err) => {
                    warn!(?err, "unable to count users");
                    UserCount(-1)
                }
            }
        } else {
            UserCount(-1)
        };

        #[derive(Default, Serialize)]
        struct RecentCounts {
            one_week: i64,
            one_day: i64,
            one_hour: i64,
            one_minute: i64,
        }

        impl RecentCounts {
            fn for_err() -> Self {
                Self {
                    one_week: -1,
                    one_day: -1,
                    one_hour: -1,
                    one_minute: -1,
                }
            }
        }

        let (recent_ip_counts, recent_user_id_counts, recent_tx_counts): (
            RecentCounts,
            RecentCounts,
            RecentCounts,
        ) = match self.redis_conn().await {
            Ok(mut redis_conn) => {
                // TODO: delete any hash entries where
                const ONE_MINUTE: i64 = 60;
                const ONE_HOUR: i64 = ONE_MINUTE * 60;
                const ONE_DAY: i64 = ONE_HOUR * 24;
                const ONE_WEEK: i64 = ONE_DAY * 7;

                let one_week_ago = Utc::now().timestamp() - ONE_WEEK;
                let one_day_ago = Utc::now().timestamp() - ONE_DAY;
                let one_hour_ago = Utc::now().timestamp() - ONE_HOUR;
                let one_minute_ago = Utc::now().timestamp() - ONE_MINUTE;

                let recent_users_by_id = format!("recent_users:id:{}", self.config.chain_id);
                let recent_users_by_ip = format!("recent_users:ip:{}", self.config.chain_id);
                let recent_transactions =
                    format!("eth_sendRawTransaction:{}", self.config.chain_id);

                match redis::pipe()
                    .atomic()
                    // delete any entries older than 1 week
                    .zrembyscore(&recent_users_by_id, i64::MIN, one_week_ago)
                    .ignore()
                    .zrembyscore(&recent_users_by_ip, i64::MIN, one_week_ago)
                    .ignore()
                    .zrembyscore(&recent_transactions, i64::MIN, one_week_ago)
                    .ignore()
                    // get counts for last week
                    .zcount(&recent_users_by_id, one_week_ago, i64::MAX)
                    .zcount(&recent_users_by_ip, one_week_ago, i64::MAX)
                    .zcount(&recent_transactions, one_week_ago, i64::MAX)
                    // get counts for last day
                    .zcount(&recent_users_by_id, one_day_ago, i64::MAX)
                    .zcount(&recent_users_by_ip, one_day_ago, i64::MAX)
                    .zcount(&recent_transactions, one_day_ago, i64::MAX)
                    // get counts for last hour
                    .zcount(&recent_users_by_id, one_hour_ago, i64::MAX)
                    .zcount(&recent_users_by_ip, one_hour_ago, i64::MAX)
                    .zcount(&recent_transactions, one_hour_ago, i64::MAX)
                    // get counts for last minute
                    .zcount(&recent_users_by_id, one_minute_ago, i64::MAX)
                    .zcount(&recent_users_by_ip, one_minute_ago, i64::MAX)
                    .zcount(&recent_transactions, one_minute_ago, i64::MAX)
                    .query_async(&mut redis_conn)
                    .await
                {
                    Ok((
                        user_id_in_week,
                        ip_in_week,
                        txs_in_week,
                        user_id_in_day,
                        ip_in_day,
                        txs_in_day,
                        user_id_in_hour,
                        ip_in_hour,
                        txs_in_hour,
                        user_id_in_minute,
                        ip_in_minute,
                        txs_in_minute,
                    )) => {
                        let recent_user_id_counts = RecentCounts {
                            one_week: user_id_in_week,
                            one_day: user_id_in_day,
                            one_hour: user_id_in_hour,
                            one_minute: user_id_in_minute,
                        };
                        let recent_ip_counts = RecentCounts {
                            one_week: ip_in_week,
                            one_day: ip_in_day,
                            one_hour: ip_in_hour,
                            one_minute: ip_in_minute,
                        };
                        let recent_tx_counts = RecentCounts {
                            one_week: txs_in_week,
                            one_day: txs_in_day,
                            one_hour: txs_in_hour,
                            one_minute: txs_in_minute,
                        };

                        (recent_ip_counts, recent_user_id_counts, recent_tx_counts)
                    }
                    Err(err) => {
                        warn!(?err, "unable to count recent users");
                        (
                            RecentCounts::for_err(),
                            RecentCounts::for_err(),
                            RecentCounts::for_err(),
                        )
                    }
                }
            }
            Err(err) => {
                warn!(?err, "unable to connect to redis while counting users");
                (
                    RecentCounts::for_err(),
                    RecentCounts::for_err(),
                    RecentCounts::for_err(),
                )
            }
        };

        #[derive(Serialize)]
        struct CombinedMetrics {
            recent_ip_counts: RecentCounts,
            recent_user_id_counts: RecentCounts,
            recent_tx_counts: RecentCounts,
            user_count: UserCount,
        }

        let metrics = CombinedMetrics {
            recent_ip_counts,
            recent_user_id_counts,
            recent_tx_counts,
            user_count,
        };

        // TODO: i don't like this library. it doesn't include HELP or TYPE lines and so our prometheus server fails to parse it
        serde_prometheus::to_string(&metrics, Some("web3_proxy"), globals)
            .expect("prometheus metrics should always serialize")
    }

    /// make an internal request with stats and caching
    pub async fn internal_request<P: JsonRpcParams, R: JsonRpcResultData>(
        self: &Arc<Self>,
        method: &str,
        params: P,
    ) -> Web3ProxyResult<R> {
        let authorization = Arc::new(Authorization::internal()?);

        self.authorized_request(method, params, authorization).await
    }

    /// this is way more round-a-bout than we want, but it means stats are emitted and caches are used
    pub async fn authorized_request<P: JsonRpcParams, R: JsonRpcResultData>(
        self: &Arc<Self>,
        method: &str,
        params: P,
        authorization: Arc<Authorization>,
    ) -> Web3ProxyResult<R> {
        // TODO: proper ids
        let request =
            SingleRequest::new(LooseId::Number(1), method.to_string().into(), json!(params))?;

        let (_, response, _) = self.proxy_request(request, authorization, None).await;

        // TODO: error handling?
        match response.parsed().await?.payload {
            jsonrpc::ResponsePayload::Success { result } => Ok(serde_json::from_str(result.get())?),
            jsonrpc::ResponsePayload::Error { error } => {
                Err(Web3ProxyError::JsonRpcErrorData(error))
            }
        }
    }

    /// send the request or batch of requests to the approriate RPCs
    pub async fn proxy_web3_rpc(
        self: &Arc<Self>,
        authorization: Arc<Authorization>,
        request: JsonRpcRequestEnum,
    ) -> Web3ProxyResult<(StatusCode, jsonrpc::Response, Vec<Arc<Web3Rpc>>)> {
        // trace!(?request, "proxy_web3_rpc");

        let response = match request {
            JsonRpcRequestEnum::Single(request) => {
                let (status_code, response, rpcs) = self
                    .proxy_request(request, authorization.clone(), None)
                    .await;

                (status_code, jsonrpc::Response::Single(response), rpcs)
            }
            JsonRpcRequestEnum::Batch(requests) => {
                let (responses, rpcs) = self
                    .proxy_web3_rpc_requests(&authorization, requests)
                    .await?;

                // TODO: real status code. if an error happens, i don't think we are following the spec here
                (StatusCode::OK, jsonrpc::Response::Batch(responses), rpcs)
            }
        };

        Ok(response)
    }

    /// cut up the request and send to potentually different servers
    /// TODO: make sure this isn't a problem
    async fn proxy_web3_rpc_requests(
        self: &Arc<Self>,
        authorization: &Arc<Authorization>,
        requests: Vec<SingleRequest>,
    ) -> Web3ProxyResult<(Vec<jsonrpc::ParsedResponse>, Vec<Arc<Web3Rpc>>)> {
        // TODO: we should probably change ethers-rs to support this directly. they pushed this off to v2 though
        let num_requests = requests.len();

        if num_requests == 0 {
            return Ok((vec![], vec![]));
        }

        // get the head block now so that any requests that need it all use the same block
        // TODO: this still has an edge condition if there is a reorg in the middle of the request!!!
        let head_block: Web3ProxyBlock = self
            .balanced_rpcs
            .head_block()
            .ok_or(Web3ProxyError::NoServersSynced)?;

        // TODO: use streams and buffers so we don't overwhelm our server
        let responses = join_all(
            requests
                .into_iter()
                .map(|request| {
                    self.proxy_request(request, authorization.clone(), Some(head_block.clone()))
                })
                .collect::<Vec<_>>(),
        )
        .await;

        let mut collected: Vec<jsonrpc::ParsedResponse> = Vec::with_capacity(num_requests);
        let mut collected_rpc_names: HashSet<String> = HashSet::new();
        let mut collected_rpcs: Vec<Arc<Web3Rpc>> = vec![];
        for response in responses {
            // TODO: any way to attach the tried rpcs to the error? it is likely helpful
            let (_status_code, response, rpcs) = response;

            // TODO: individual error handling
            collected.push(response.parsed().await?);
            collected_rpcs.extend(rpcs.into_iter().filter(|x| {
                if collected_rpc_names.contains(&x.name) {
                    false
                } else {
                    collected_rpc_names.insert(x.name.clone());
                    true
                }
            }));

            // TODO: what should we do with the status code? check the jsonrpc spec
        }

        Ok((collected, collected_rpcs))
    }

    pub async fn redis_conn(&self) -> Web3ProxyResult<redis_rate_limiter::RedisConnection> {
        match self.vredis_pool.as_ref() {
            None => Err(Web3ProxyError::NoDatabaseConfigured),
            Some(redis_pool) => {
                // TODO: add a From for this
                let redis_conn = redis_pool.get().await.context("redis pool error")?;

                Ok(redis_conn)
            }
        }
    }

    /// try to send transactions to the best available rpcs with protected/private mempools
    /// if no protected rpcs are configured (and protected_only is false), then public rpcs are used instead
    /// TODO: should this return an H256 instead of an Arc<RawValue>?
    async fn try_send_protected(
        self: &Arc<Self>,
        web3_request: &Arc<ValidatedRequest>,
        protected_only: bool,
    ) -> Web3ProxyResult<ForwardedResponse<Arc<RawValue>>> {
        // decode the transaction
        let params = web3_request
            .inner
            .params()
            .as_array()
            .ok_or_else(|| Web3ProxyError::BadRequest("Unable to get array from params".into()))?
            .get(0)
            .ok_or_else(|| Web3ProxyError::BadRequest("Unable to get item 0 from params".into()))?
            .as_str()
            .ok_or_else(|| {
                Web3ProxyError::BadRequest("Unable to get string from params item 0".into())
            })?;

        let bytes = Bytes::from_str(params)
            .map_err(|_| Web3ProxyError::BadRequest("Unable to parse params as bytes".into()))?;

        if bytes.is_empty() {
            return Err(Web3ProxyError::BadRequest("empty bytes".into()));
        }

        let rlp = Rlp::new(bytes.as_ref());

        let tx = Transaction::decode(&rlp).map_err(|_| {
            Web3ProxyError::BadRequest("failed to parse rlp into transaction".into())
        })?;

        if let Some(chain_id) = tx.chain_id {
            if self.config.chain_id != chain_id.as_u64() {
                return Err(Web3ProxyError::BadRequest(
                    format!(
                        "unexpected chain_id. {} != {}",
                        chain_id, self.config.chain_id
                    )
                    .into(),
                ));
            }
        }

        // TODO: return now if already confirmed
        // TODO: error if the nonce is way far in the future

        let mut response = if protected_only {
            if self.protected_rpcs.is_empty() {
                // TODO: different error?
                return Err(Web3ProxyError::NoServersSynced);
            }
            self.protected_rpcs
                .request_with_metadata(web3_request)
                .await
        } else if self.protected_rpcs.is_empty() {
            self.balanced_rpcs.request_with_metadata(web3_request).await
        } else {
            self.protected_rpcs
                .request_with_metadata(web3_request)
                .await
        };

        // TODO: helper for doing parsed() inside a response?
        if let Ok(SingleResponse::Stream(x)) = response {
            response = x
                .read()
                .await
                .map(SingleResponse::Parsed)
                .map_err(Into::into);
        }

        let mut response = response.try_into()?;

        let txid = tx.hash();

        // sometimes we get an error that the transaction is already known by our nodes,
        // that's not really an error. Return the hash like a successful response would.
        // TODO: move this to a helper function. probably part of try_send_protected
        if let ForwardedResponse::RpcError { error_data, .. } = &response {
            let acceptable_error_messages = [
                "already known",
                "ALREADY_EXISTS: already known",
                "INTERNAL_ERROR: existing tx with same hash",
                "",
            ];
            if acceptable_error_messages.contains(&error_data.message.as_ref()) {
                response = ForwardedResponse::from(json!(txid));
            }
        }

        // if successful, send the txid to the pending transaction firehose
        if let ForwardedResponse::Result { value, .. } = &response {
            // no idea how we got an array here, but lets force this to just the txid
            // TODO: think about this more
            if value.get().starts_with('[') {
                let backend_rpcs = web3_request
                    .backend_rpcs_used()
                    .iter()
                    .map(|x| x.name.as_str())
                    .collect::<Vec<_>>();

                error!(
                    ?value,
                    ?txid,
                    ?backend_rpcs,
                    "unexpected array response from sendRawTransaction"
                );
                response = ForwardedResponse::from(json!(txid));
            }

            self.pending_txid_firehose.send(txid).await;

            // emit transaction count stats
            // TODO: different salt for ips and transactions?
            if let Some(ref salt) = self.config.public_recent_ips_salt {
                let now = Utc::now().timestamp();
                let app = self.clone();

                let salted_tx_hash = format!("{}:{:?}", salt, txid);

                let f = async move {
                    match app.redis_conn().await {
                        Ok(mut redis_conn) => {
                            let hashed_tx_hash = Bytes::from(keccak256(salted_tx_hash.as_bytes()));

                            let recent_tx_hash_key =
                                format!("eth_sendRawTransaction:{}", app.config.chain_id);

                            redis_conn
                                .zadd(recent_tx_hash_key, hashed_tx_hash.to_string(), now)
                                .await?;
                        }
                        Err(Web3ProxyError::NoDatabaseConfigured) => {}
                        Err(err) => {
                            warn!(?err, "unable to save stats for eth_sendRawTransaction",)
                        }
                    }

                    Ok::<_, anyhow::Error>(())
                };

                tokio::spawn(f);
            }
        }

        Ok(response)
    }

    /// proxy request with up to 3 tries.
    async fn proxy_request(
        self: &Arc<Self>,
        request: SingleRequest,
        authorization: Arc<Authorization>,
        head_block: Option<Web3ProxyBlock>,
    ) -> (StatusCode, jsonrpc::SingleResponse, Vec<Arc<Web3Rpc>>) {
        // TODO: this clone is only for an error response. refactor to not need it
        let error_id = request.id.clone();

        let mut last_success = None;
        let mut last_error = None;
        let mut web3_request;

        // TODO: think more about how to handle retries without hammering our servers with errors
        let mut ranked_rpcs = self.balanced_rpcs.watch_ranked_rpcs.subscribe();

        let latest_start = sleep_until(Instant::now() + Duration::from_secs(3));
        pin!(latest_start);

        // TODO: how many retries?
        loop {
            // TODO: refresh the request instead of making new each time. then we need less clones
            web3_request = match ValidatedRequest::new_with_app(
                self,
                authorization.clone(),
                None,
                None,
                request.clone().into(),
                head_block.clone(),
            )
            .await
            {
                Ok(x) => x,
                Err(err) => {
                    let (a, b) = err.as_json_response_parts(error_id);

                    let rpcs = vec![];

                    return (a, b, rpcs);
                }
            };

            // turn some of the Web3ProxyErrors into Ok results
            match self._proxy_request_with_caching(&web3_request).await {
                Ok(response_data) => {
                    last_success = Some(response_data);
                    break;
                }
                Err(err) => {
                    last_error = Some(err);
                }
            }

            select! {
                _ = ranked_rpcs.changed() => {
                    // TODO: pass these RankedRpcs to ValidatedRequest::new_with_app
                    ranked_rpcs.borrow_and_update();
                }
                _ = &mut latest_start => {
                    // do not retry if we've already been trying for 3 seconds
                    break;
                }
            }
        }

        let last_response = if let Some(last_success) = last_success {
            Ok(last_success)
        } else {
            Err(last_error.unwrap_or(anyhow::anyhow!("no success or error").into()))
        };

        let (code, response) = match last_response {
            Ok(response_data) => {
                web3_request.error_response.store(false, Ordering::Relaxed);

                // TODO: is it true that all jsonrpc errors are user errors?
                web3_request
                    .user_error_response
                    .store(response_data.is_jsonrpc_err(), Ordering::Relaxed);

                (StatusCode::OK, response_data)
            }
            Err(err) => {
                // max tries exceeded. return the error

                web3_request.error_response.store(true, Ordering::Relaxed);
                web3_request
                    .user_error_response
                    .store(false, Ordering::Relaxed);

                err.as_json_response_parts(web3_request.id())
            }
        };

        web3_request.add_response(&response);

        let rpcs = web3_request.backend_rpcs_used();

        (code, response, rpcs)
    }

    /// main logic for proxy_cached_request but in a dedicated function so the try operator is easy to use
    /// TODO: how can we make this generic?
    async fn _proxy_request_with_caching(
        self: &Arc<Self>,
        web3_request: &Arc<ValidatedRequest>,
    ) -> Web3ProxyResult<jsonrpc::SingleResponse> {
        // TODO: serve net_version without querying the backend
        // TODO: don't force RawValue
        let response: jsonrpc::SingleResponse = match web3_request.inner.method() {
            // lots of commands are blocked
            method @ ("db_getHex"
            | "db_getString"
            | "db_putHex"
            | "db_putString"
            | "debug_accountRange"
            | "debug_backtraceAt"
            | "debug_blockProfile"
            | "debug_bundler_clearState"
            | "debug_bundler_dumpMempool"
            | "debug_bundler_sendBundleNow"
            | "debug_chaindbCompact"
            | "debug_chaindbProperty"
            | "debug_cpuProfile"
            | "debug_freeOSMemory"
            | "debug_freezeClient"
            | "debug_gcStats"
            | "debug_goTrace"
            | "debug_memStats"
            | "debug_mutexProfile"
            | "debug_setBlockProfileRate"
            | "debug_setGCPercent"
            | "debug_setHead"
            | "debug_setMutexProfileFraction"
            | "debug_standardTraceBadBlockToFile"
            | "debug_standardTraceBlockToFile"
            | "debug_startCPUProfile"
            | "debug_startGoTrace"
            | "debug_stopCPUProfile"
            | "debug_stopGoTrace"
            | "debug_writeBlockProfile"
            | "debug_writeMemProfile"
            | "debug_writeMutexProfile"
            | "erigon_cacheCheck"
            | "eth_compileLLL"
            | "eth_compileSerpent"
            | "eth_compileSolidity"
            | "eth_getCompilers"
            | "eth_sendTransaction"
            | "eth_sign"
            | "eth_signTransaction"
            | "eth_submitHashrate"
            | "eth_submitWork"
            | "les_addBalance"
            | "les_setClientParams"
            | "les_setDefaultParams"
            | "miner_setEtherbase"
            | "miner_setExtra"
            | "miner_setGasLimit"
            | "miner_setGasPrice"
            | "miner_start"
            | "miner_stop"
            | "personal_ecRecover"
            | "personal_importRawKey"
            | "personal_listAccounts"
            | "personal_lockAccount"
            | "personal_newAccount"
            | "personal_sendTransaction"
            | "personal_sign"
            | "personal_unlockAccount"
            | "shh_addToGroup"
            | "shh_getFilterChanges"
            | "shh_getMessages"
            | "shh_hasIdentity"
            | "shh_newFilter"
            | "shh_newGroup"
            | "shh_newIdentity"
            | "shh_post"
            | "shh_uninstallFilter"
            | "shh_version") => {
                return Err(Web3ProxyError::MethodNotFound(method.to_owned().into()));
            }
            // TODO: implement these commands
            method @ ("eth_getFilterChanges"
            | "eth_getFilterLogs"
            | "eth_newBlockFilter"
            | "eth_newFilter"
            | "eth_newPendingTransactionFilter"
            | "eth_pollSubscriptions"
            | "eth_uninstallFilter") => {
                return Err(Web3ProxyError::MethodNotFound(method.to_owned().into()));
            }
            "eth_sendUserOperation"
            | "eth_estimateUserOperationGas"
            | "eth_getUserOperationByHash"
            | "eth_getUserOperationReceipt"
            | "eth_supportedEntryPoints"
            | "web3_bundlerVersion" => self.bundler_4337_rpcs
                        .try_proxy_connection::<Arc<RawValue>>(
                            web3_request,
                        )
                        .await?,
            "eth_accounts" => jsonrpc::ParsedResponse::from_value(serde_json::Value::Array(vec![]), web3_request.id()).into(),
            "eth_blockNumber" => {
                match web3_request.head_block.clone().or(self.balanced_rpcs.head_block()) {
                    Some(head_block) => jsonrpc::ParsedResponse::from_value(json!(head_block.number()), web3_request.id()).into(),
                    None => {
                        return Err(Web3ProxyError::NoServersSynced);
                    }
                }
            }
            "eth_chainId" => jsonrpc::ParsedResponse::from_value(json!(U64::from(self.config.chain_id)), web3_request.id()).into(),
            // TODO: eth_callBundle (https://docs.flashbots.net/flashbots-auction/searchers/advanced/rpc-endpoint#eth_callbundle)
            // TODO: eth_cancelPrivateTransaction (https://docs.flashbots.net/flashbots-auction/searchers/advanced/rpc-endpoint#eth_cancelprivatetransaction, but maybe just reject)
            // TODO: eth_sendPrivateTransaction (https://docs.flashbots.net/flashbots-auction/searchers/advanced/rpc-endpoint#eth_sendprivatetransaction)
            "eth_coinbase" => {
                // no need for serving coinbase
                jsonrpc::ParsedResponse::from_value(json!(Address::zero()), web3_request.id()).into()
            }
            "eth_estimateGas" => {
                // TODO: timeout
                let mut gas_estimate = self
                    .balanced_rpcs
                    .try_proxy_connection::<U256>(
                        web3_request,
                    )
                    .await?
                    .parsed()
                    .await?
                    .into_result()?;

                let gas_increase = if let Some(gas_increase_percent) =
                    self.config.gas_increase_percent
                {
                    let gas_increase = gas_estimate * gas_increase_percent / U256::from(100);

                    let min_gas_increase = self.config.gas_increase_min.unwrap_or_default();

                    gas_increase.max(min_gas_increase)
                } else {
                    self.config.gas_increase_min.unwrap_or_default()
                };

                gas_estimate += gas_increase;

                let request_id = web3_request.id();

                // TODO: from_serializable?
                jsonrpc::ParsedResponse::from_value(json!(gas_estimate), request_id).into()
            }
            "eth_getTransactionReceipt" | "eth_getTransactionByHash" => {
                // try to get the transaction without specifying a min_block_height
                // TODO: timeout
                // TODO: change this to send serially until we get a success

                // TODO: validate params. we seem to get a lot of spam here of "0x"

                let mut result = self
                    .balanced_rpcs
                    .try_proxy_connection::<Arc<RawValue>>(
                        web3_request,
                    )
                    .await;

                // TODO: helper for doing parsed() inside a result?
                if let Ok(SingleResponse::Stream(x)) = result {
                    result = x.read().await.map(SingleResponse::Parsed).map_err(Into::into);
                }

                // if we got "null" or "", it is probably because the tx is old. retry on nodes with old block data
                // TODO: this feels fragile. how should we do this better/
                let try_archive = match &result {
                    Ok(SingleResponse::Parsed(x)) => {
                        let x = x.result().map(|x| json!(x));

                        match x {
                            Some(serde_json::Value::Null) => true,
                            Some(serde_json::Value::Array(x)) => x.is_empty(),
                            Some(serde_json::Value::String(x)) => x.is_empty(),
                            None => true,
                            _ => false,
                        }
                    },
                    Ok(SingleResponse::Stream(..)) => unimplemented!(),
                    Err(..) => true,
                };

                if try_archive {
                    // TODO: only charge for archive if it gave a result
                    web3_request
                        .archive_request
                        .store(true, atomic::Ordering::Relaxed);

                    // TODO: we don't actually want try_send_all. we want the first non-null, non-error response
                    self
                        .balanced_rpcs
                        .try_proxy_connection::<Arc<RawValue>>(
                            web3_request,
                        )
                        .await?
                } else {

                    // TODO: if result is an error, return a null instead?

                    result?
                }
            }
            // TODO: eth_gasPrice that does awesome magic to predict the future
            "eth_hashrate" => jsonrpc::ParsedResponse::from_value(json!(U64::zero()), web3_request.id()).into(),
            "eth_mining" => jsonrpc::ParsedResponse::from_value(serde_json::Value::Bool(false), web3_request.id()).into(),
            "eth_sendRawTransaction" => {
                // TODO: eth_sendPrivateTransaction that only sends private and never to balanced. it has different params though
                let x = self
                    .try_send_protected(
                        web3_request,false,
                    ).await?;

                jsonrpc::ParsedResponse::from_response_data(x, web3_request.id()).into()
            }
            "eth_syncing" => {
                // no stats on this. its cheap
                // TODO: return a real response if all backends are syncing or if no servers in sync
                // TODO: const
                jsonrpc::ParsedResponse::from_value(serde_json::Value::Bool(false), web3_request.id()).into()
            }
            "eth_subscribe" => jsonrpc::ParsedResponse::from_error(JsonRpcErrorData {
                message: "notifications not supported. eth_subscribe is only available over a websocket".into(),
                code: -32601,
                data: None,
            }, web3_request.id()).into(),
            "eth_unsubscribe" => jsonrpc::ParsedResponse::from_error(JsonRpcErrorData {
                message: "notifications not supported. eth_unsubscribe is only available over a websocket".into(),
                code: -32601,
                data: None,
            }, web3_request.id()).into(),
            "net_listening" => {
                // TODO: only true if there are some backends on balanced_rpcs?
                // TODO: const
                jsonrpc::ParsedResponse::from_value(serde_json::Value::Bool(true), web3_request.id()).into()
            }
            "net_peerCount" =>
                jsonrpc::ParsedResponse::from_value(json!(U64::from(self.balanced_rpcs.num_synced_rpcs())), web3_request.id()).into()
            ,
            "web3_clientVersion" =>
                jsonrpc::ParsedResponse::from_value(serde_json::Value::String(APP_USER_AGENT.to_string()), web3_request.id()).into()
            ,
            "web3_sha3" => {
                // returns Keccak-256 (not the standardized SHA3-256) of the given data.
                // TODO: timeout
                match &web3_request.inner.params() {
                    serde_json::Value::Array(params) => {
                        // TODO: make a struct and use serde conversion to clean this up
                        if params.len() != 1
                            || !params.get(0).map(|x| x.is_string()).unwrap_or(false)
                        {
                            // TODO: what error code?
                            // TODO: use Web3ProxyError::BadRequest
                            jsonrpc::ParsedResponse::from_error(JsonRpcErrorData {
                                message: "Invalid request".into(),
                                code: -32600,
                                data: None
                            }, web3_request.id()).into()
                        } else {
                            // TODO: BadRequest instead of web3_context
                            let param = Bytes::from_str(
                                params[0]
                                    .as_str()
                                    .ok_or(Web3ProxyError::ParseBytesError(None))
                                    .web3_context("parsing params 0 into str then bytes")?,
                            )
                            .map_err(|x| {
                                trace!("bad request: {:?}", x);
                                Web3ProxyError::BadRequest(
                                    "param 0 could not be read as H256".into(),
                                )
                            })?;

                            let hash = H256::from(keccak256(param));

                            jsonrpc::ParsedResponse::from_value(json!(hash), web3_request.id()).into()
                        }
                    }
                    _ => {
                        // TODO: this needs the correct error code in the response
                        // TODO: Web3ProxyError::BadRequest instead?
                        jsonrpc::ParsedResponse::from_error(JsonRpcErrorData {
                            message: "invalid request".into(),
                            code: StatusCode::BAD_REQUEST.as_u16().into(),
                            data: None,
                        }, web3_request.id()).into()
                    }
                }
            }
            "test" => jsonrpc::ParsedResponse::from_error(JsonRpcErrorData {
                message: "The method test does not exist/is not available.".into(),
                code: -32601,
                data: None,
            }, web3_request.id()).into(),
            // anything else gets sent to backend rpcs and cached
            method => {
                if method.starts_with("admin_") {
                    // TODO: emit a stat? will probably just be noise
                    return Err(Web3ProxyError::AccessDenied("admin methods are not allowed".into()));
                }
                if method.starts_with("alchemy_") {
                    return Err(JsonRpcErrorData::from(format!(
                        "the method {} does not exist/is not available",
                        method
                    )).into());
                }
                // debug methods require premium
                if method.starts_with("debug_") && !(self.config.free_subscriptions
                        || web3_request.authorization.active_premium().await) {
                        return Err(Web3ProxyError::AccessDenied(
                            "debug methods require an active premium account".into(),
                        ));
                    }

                if web3_request.cache_mode.is_some() {
                    // don't cache anything larger than 16 MiB
                    let max_response_cache_bytes = 16 * (1024 ^ 2);  // self.config.max_response_cache_bytes;

                    let cache_key = web3_request.cache_key().expect("key must exist if cache_mode does");

                    // TODO: try to fetch out of s3

                    let x: SingleResponse = if let Some(data) = self.jsonrpc_response_cache.get(&cache_key).await {
                        // it was cached! easy!
                        jsonrpc::ParsedResponse::from_response_data(data, web3_request.id()).into()
                    } else if self.jsonrpc_response_failed_cache_keys.contains_key(&cache_key) {
                        // this is a request that we have previously failed to cache. don't try the cache again
                        // TODO: is "contains_key" okay, or do we need "get($cache_key).await"?
                        // TODO: DRY. we do this timeout and try_proxy_connection below, too.
                        timeout_at(
                            web3_request.expire_at(),
                            self.balanced_rpcs
                            .try_proxy_connection::<Arc<RawValue>>(
                                web3_request,
                            )
                        ).await??
                    } else {
                        // we used to have a semaphore here, but its faster to just allow duplicate requests while the first is still in flight
                        // we might do some duplicate requests here, but it seems worth it to get rid of the Arc errors.
                        let response_data = timeout_at(
                            web3_request.expire_at(),
                            self.balanced_rpcs
                            .try_proxy_connection::<Arc<RawValue>>(
                                web3_request,
                            )
                        ).await?;

                        match response_data {
                            Ok(mut x) => {
                                match &x {
                                    SingleResponse::Parsed(x) => {
                                        // TODO: don't serialize here! we should already know the size!
                                        let len = serde_json::to_string(&x).unwrap().len();

                                        if len <= max_response_cache_bytes {
                                            let cached = ForwardedResponse::from(x.payload.clone());

                                            self.jsonrpc_response_cache.insert(cache_key, cached).await;
                                        } else {
                                            self.jsonrpc_response_failed_cache_keys.insert(cache_key, ()).await;
                                        }
                                    }
                                    SingleResponse::Stream(..) => {
                                        self.jsonrpc_response_failed_cache_keys.insert(cache_key, ()).await;
                                    }
                                }

                                x.set_id(web3_request.id());

                                x
                            }
                            Err(err) => {
                                if web3_request.cache_jsonrpc_errors() {
                                    // we got an error, but we are supposed to cache jsonrpc errors. 
                                    let x: Result<ForwardedResponse<Arc<RawValue>>, Web3ProxyError> = err.try_into();

                                    if x.is_err() {
                                        // we still have an Err. it must not have been a jsonrpc error
                                        self.jsonrpc_response_failed_cache_keys.insert(cache_key, ()).await;
                                    }

                                    // TODO: needing multiple into/try_into/from must be inefficient. investigate this
                                    ParsedResponse::from_response_data(x?, web3_request.id()).into()
                                } else {
                                    // we got an error, and we are not supposed to cache jsonrpc errors. exit early
                                    self.jsonrpc_response_failed_cache_keys.insert(cache_key, ()).await;
                                    return Err(err);
                                }
                            }
                        }
                    };

                    x
                } else {
                    let mut x = timeout_at(
                        web3_request.expire_at(),
                        self.balanced_rpcs
                        .try_proxy_connection::<Arc<RawValue>>(
                            web3_request,
                        )
                    ).await??;

                    x.set_id(web3_request.id());

                    x
                }
            }
        };

        Ok(response)
    }
}

impl fmt::Debug for App {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("Web3ProxyApp").finish_non_exhaustive()
    }
}
