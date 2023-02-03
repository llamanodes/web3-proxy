// TODO: this file is way too big now. move things into other modules
mod ws;

use crate::app_stats::{ProxyResponseStat, StatEmitter, Web3ProxyStat};
use crate::block_number::{block_needed, BlockNeeded};
use crate::config::{AppConfig, TopConfig};
use crate::frontend::authorization::{Authorization, RequestMetadata, RpcSecretKey};
use crate::frontend::errors::FrontendErrorResponse;
use crate::frontend::rpc_proxy_ws::ProxyMode;
use crate::jsonrpc::{
    JsonRpcForwardedResponse, JsonRpcForwardedResponseEnum, JsonRpcRequest, JsonRpcRequestEnum,
};
use crate::rpcs::blockchain::{ArcBlock, SavedBlock};
use crate::rpcs::connection::Web3Connection;
use crate::rpcs::connections::Web3Connections;
use crate::rpcs::request::OpenRequestHandleMetrics;
use crate::rpcs::transactions::TxStatus;
use crate::user_token::UserBearerToken;
use anyhow::Context;
use axum::headers::{Origin, Referer, UserAgent};
use chrono::Utc;
use deferred_rate_limiter::DeferredRateLimiter;
use derive_more::From;
use entities::sea_orm_active_enums::LogLevel;
use entities::user;
use ethers::core::utils::keccak256;
use ethers::prelude::{Address, Block, Bytes, Transaction, TxHash, H256, U64};
use ethers::types::U256;
use ethers::utils::rlp::{Decodable, Rlp};
use futures::future::join_all;
use futures::stream::{FuturesUnordered, StreamExt};
use hashbrown::{HashMap, HashSet};
use ipnet::IpNet;
use tracing::{debug, error, info, instrument, trace, warn, Level};
use metered::{metered, ErrorCount, HitCount, ResponseTime, Throughput};
use migration::sea_orm::{
    self, ConnectionTrait, Database, DatabaseConnection, EntityTrait, PaginatorTrait,
};
use migration::sea_query::table::ColumnDef;
use migration::{Alias, DbErr, Migrator, MigratorTrait, Table};
use moka::future::Cache;
use redis_rate_limiter::redis::AsyncCommands;
use redis_rate_limiter::{redis, DeadpoolRuntime, RedisConfig, RedisPool, RedisRateLimiter};
use serde::Serialize;
use serde_json::json;
use serde_json::value::to_raw_value;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::net::IpAddr;
use std::num::NonZeroU64;
use std::str::FromStr;
use std::sync::{atomic, Arc};
use std::time::Duration;
use tokio::sync::{broadcast, watch, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use ulid::Ulid;

// TODO: make this customizable?
// TODO: include GIT_REF in here. i had trouble getting https://docs.rs/vergen/latest/vergen/ to work with a workspace. also .git is in .dockerignore
pub static APP_USER_AGENT: &str = concat!(
    "llamanodes_",
    env!("CARGO_PKG_NAME"),
    "/v",
    env!("CARGO_PKG_VERSION")
);

/// TODO: allow customizing the request period?
pub static REQUEST_PERIOD: u64 = 60;

#[derive(Debug, From)]
struct ResponseCacheKey {
    // if none, this is cached until evicted
    block: Option<SavedBlock>,
    method: String,
    // TODO: better type for this
    params: Option<serde_json::Value>,
    cache_errors: bool,
}

impl ResponseCacheKey {
    #[instrument(level = "trace")]
    fn weight(&self) -> usize {
        let mut w = self.method.len();

        if let Some(p) = self.params.as_ref() {
            w += p.to_string().len();
        }

        w
    }
}

impl PartialEq for ResponseCacheKey {
    #[instrument(level = "trace")]
    fn eq(&self, other: &Self) -> bool {
        if self.cache_errors != other.cache_errors {
            return false;
        }

        match (self.block.as_ref(), other.block.as_ref()) {
            (None, None) => {}
            (None, Some(_)) => {
                return false;
            }
            (Some(_), None) => {
                return false;
            }
            (Some(s), Some(o)) => {
                if s != o {
                    return false;
                }
            }
        }

        if self.method != other.method {
            return false;
        }

        self.params == other.params
    }
}

impl Eq for ResponseCacheKey {}

impl Hash for ResponseCacheKey {
    #[instrument(level = "trace")]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.block.as_ref().map(|x| x.hash()).hash(state);
        self.method.hash(state);
        self.params.as_ref().map(|x| x.to_string()).hash(state);
        self.cache_errors.hash(state)
    }
}

type ResponseCache =
    Cache<ResponseCacheKey, JsonRpcForwardedResponse, hashbrown::hash_map::DefaultHashBuilder>;

pub type AnyhowJoinHandle<T> = JoinHandle<anyhow::Result<T>>;

#[derive(Clone, Debug, Default, From)]
pub struct AuthorizationChecks {
    /// database id of the primary user. 0 if anon
    /// TODO: do we need this? its on the authorization so probably not
    pub user_id: u64,
    /// the key used (if any)
    pub rpc_secret_key: Option<RpcSecretKey>,
    /// database id of the rpc key
    /// if this is None, then this request is being rate limited by ip
    pub rpc_secret_key_id: Option<NonZeroU64>,
    /// if None, allow unlimited queries. inherited from the user_tier
    pub max_requests_per_period: Option<u64>,
    // if None, allow unlimited concurrent requests. inherited from the user_tier
    pub max_concurrent_requests: Option<u32>,
    /// if None, allow any Origin
    pub allowed_origins: Option<Vec<Origin>>,
    /// if None, allow any Referer
    pub allowed_referers: Option<Vec<Referer>>,
    /// if None, allow any UserAgent
    pub allowed_user_agents: Option<Vec<UserAgent>>,
    /// if None, allow any IP Address
    pub allowed_ips: Option<Vec<IpNet>>,
    pub log_level: LogLevel,
    /// Chance to save reverting eth_call, eth_estimateGas, and eth_sendRawTransaction to the database.
    /// TODO: f32 would be fine
    pub log_revert_chance: f64,
    /// if true, transactions are broadcast to private mempools. They will still be public on the blockchain!
    pub private_txs: bool,
}

/// Simple wrapper so that we can keep track of read only connections.
/// This does no blocking of writing in the compiler!
#[derive(Clone, Debug)]
pub struct DatabaseReplica(pub DatabaseConnection);

// TODO: I feel like we could do something smart with DeRef or AsRef or Borrow, but that wasn't working for me
impl DatabaseReplica {
    #[instrument(skip_all)]
    pub fn conn(&self) -> &DatabaseConnection {
        &self.0
    }
}

/// The application
// TODO: this debug impl is way too verbose. make something smaller
// TODO: i'm sure this is more arcs than necessary, but spawning futures makes references hard
pub struct Web3ProxyApp {
    /// Send requests to the best server available
    pub balanced_rpcs: Arc<Web3Connections>,
    /// Send private requests (like eth_sendRawTransaction) to all these servers
    pub private_rpcs: Option<Arc<Web3Connections>>,
    response_cache: ResponseCache,
    // don't drop this or the sender will stop working
    // TODO: broadcast channel instead?
    watch_consensus_head_receiver: watch::Receiver<ArcBlock>,
    pending_tx_sender: broadcast::Sender<TxStatus>,
    pub config: AppConfig,
    pub db_conn: Option<sea_orm::DatabaseConnection>,
    pub db_replica: Option<DatabaseReplica>,
    /// prometheus metrics
    app_metrics: Arc<Web3ProxyAppMetrics>,
    open_request_handle_metrics: Arc<OpenRequestHandleMetrics>,
    /// store pending transactions that we've seen so that we don't send duplicates to subscribers
    pub pending_transactions: Cache<TxHash, TxStatus, hashbrown::hash_map::DefaultHashBuilder>,
    pub frontend_ip_rate_limiter: Option<DeferredRateLimiter<IpAddr>>,
    pub frontend_registered_user_rate_limiter: Option<DeferredRateLimiter<u64>>,
    pub login_rate_limiter: Option<RedisRateLimiter>,
    pub vredis_pool: Option<RedisPool>,
    // TODO: this key should be our RpcSecretKey class, not Ulid
    pub rpc_secret_key_cache:
        Cache<Ulid, AuthorizationChecks, hashbrown::hash_map::DefaultHashBuilder>,
    pub registered_user_semaphores:
        Cache<NonZeroU64, Arc<Semaphore>, hashbrown::hash_map::DefaultHashBuilder>,
    pub ip_semaphores: Cache<IpAddr, Arc<Semaphore>, hashbrown::hash_map::DefaultHashBuilder>,
    pub bearer_token_semaphores:
        Cache<UserBearerToken, Arc<Semaphore>, hashbrown::hash_map::DefaultHashBuilder>,
    pub stat_sender: Option<flume::Sender<Web3ProxyStat>>,
}

/// flatten a JoinError into an anyhow error
/// Useful when joining multiple futures.
#[instrument(skip_all)]
pub async fn flatten_handle<T>(handle: AnyhowJoinHandle<T>) -> anyhow::Result<T> {
    match handle.await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(err)) => Err(err),
        Err(err) => Err(err.into()),
    }
}

/// return the first error or okay if everything worked
#[instrument(skip_all)]
pub async fn flatten_handles<T>(
    mut handles: FuturesUnordered<AnyhowJoinHandle<T>>,
) -> anyhow::Result<()> {
    while let Some(x) = handles.next().await {
        match x {
            Err(e) => return Err(e.into()),
            Ok(Err(e)) => return Err(e),
            Ok(Ok(_)) => continue,
        }
    }

    Ok(())
}

#[instrument(level = "trace")]
pub async fn get_db(
    db_url: String,
    min_connections: u32,
    max_connections: u32,
) -> Result<DatabaseConnection, DbErr> {
    // TODO: scrub credentials and then include the db_url in logs
    info!("Connecting to db");

    let mut db_opt = sea_orm::ConnectOptions::new(db_url);

    // TODO: load all these options from the config file. i think mysql default max is 100
    // TODO: sqlx logging only in debug. way too verbose for production
    db_opt
        .connect_timeout(Duration::from_secs(30))
        .min_connections(min_connections)
        .max_connections(max_connections)
        .sqlx_logging(false);
    // .sqlx_logging_level(log::LevelFilter::Info);

    Database::connect(db_opt).await
}

#[instrument(level = "trace")]
pub async fn drop_migration_lock(db_conn: &DatabaseConnection) -> Result<(), DbErr> {
    let db_backend = db_conn.get_database_backend();

    let drop_lock_statment = db_backend.build(Table::drop().table(Alias::new("migration_lock")));

    db_conn.execute(drop_lock_statment).await?;

    debug!("migration lock unlocked");

    Ok(())
}

/// Be super careful with override_existing_lock! It is very important that only one process is running the migrations at a time!
#[instrument(level = "trace")]
pub async fn migrate_db(
    db_conn: &DatabaseConnection,
    override_existing_lock: bool,
) -> Result<(), DbErr> {
    let db_backend = db_conn.get_database_backend();

    // TODO: put the timestamp and hostname into this as columns?
    let create_lock_statment = db_backend.build(
        Table::create()
            .table(Alias::new("migration_lock"))
            .col(ColumnDef::new(Alias::new("locked")).boolean().default(true)),
    );

    loop {
        if Migrator::get_pending_migrations(&db_conn).await?.is_empty() {
            info!("no migrations to apply");
            return Ok(());
        }

        // there are migrations to apply
        // acquire a lock
        if let Err(err) = db_conn.execute(create_lock_statment.clone()).await {
            if override_existing_lock {
                warn!("OVERRIDING EXISTING LOCK in 10 seconds! ctrl+c now if other migrations are actually running!");

                sleep(Duration::from_secs(10)).await
            } else {
                debug!("Unable to acquire lock. if you are positive no migration is running, run \"web3_proxy_cli drop_migration_lock\". err={:?}", err);

                // TODO: exponential backoff with jitter?
                sleep(Duration::from_secs(1)).await;

                continue;
            }
        }

        debug!("migration lock acquired");
        break;
    }

    let migration_result = Migrator::up(&db_conn, None).await;

    // drop the distributed lock
    drop_migration_lock(&db_conn).await?;

    // return if migrations erred
    migration_result
}

/// Connect to the database and run migrations
#[instrument(level = "trace")]
pub async fn get_migrated_db(
    db_url: String,
    min_connections: u32,
    max_connections: u32,
) -> Result<DatabaseConnection, DbErr> {
    // TODO: this seems to fail silently
    let db_conn = get_db(db_url, min_connections, max_connections).await?;

    migrate_db(&db_conn, false).await?;

    Ok(db_conn)
}

#[derive(From)]
pub struct Web3ProxyAppSpawn {
    /// the app. probably clone this to use in other groups of handles
    pub app: Arc<Web3ProxyApp>,
    // cancellable handles
    pub app_handles: FuturesUnordered<AnyhowJoinHandle<()>>,
    /// these are important and must be allowed to finish
    pub background_handles: FuturesUnordered<AnyhowJoinHandle<()>>,
}

#[metered(registry = Web3ProxyAppMetrics, registry_expr = self.app_metrics, visibility = pub)]
impl Web3ProxyApp {
    /// The main entrypoint.
    #[instrument(level = "trace")]
    pub async fn spawn(
        top_config: TopConfig,
        num_workers: usize,
        shutdown_receiver: broadcast::Receiver<()>,
    ) -> anyhow::Result<Web3ProxyAppSpawn> {
        // safety checks on the config
        if let Some(redirect) = &top_config.app.redirect_rpc_key_url {
            assert!(
                redirect.contains("{{rpc_key_id}}"),
                "redirect_rpc_key_url user url must contain \"{{rpc_key_id}}\""
            );
        }

        if !top_config.extra.is_empty() {
            warn!(
                "unknown TopConfig fields!: {:?}",
                top_config.app.extra.keys()
            );
        }

        if !top_config.app.extra.is_empty() {
            warn!(
                "unknown Web3ProxyAppConfig fields!: {:?}",
                top_config.app.extra.keys()
            );
        }

        // setup metrics
        let app_metrics = Default::default();
        let open_request_handle_metrics: Arc<OpenRequestHandleMetrics> = Default::default();

        let mut db_conn = None::<DatabaseConnection>;
        let mut db_replica = None::<DatabaseReplica>;

        // connect to mysql and make sure the latest migrations have run
        if let Some(db_url) = top_config.app.db_url.clone() {
            let db_min_connections = top_config
                .app
                .db_min_connections
                .unwrap_or(num_workers as u32);

            // TODO: what default multiple?
            let db_max_connections = top_config
                .app
                .db_max_connections
                .unwrap_or(db_min_connections * 2);

            db_conn = Some(
                get_migrated_db(db_url.clone(), db_min_connections, db_max_connections).await?,
            );

            db_replica = if let Some(db_replica_url) = top_config.app.db_replica_url.clone() {
                if db_replica_url == db_url {
                    // url is the same. do not make a new connection or we might go past our max connections
                    db_conn.clone().map(DatabaseReplica)
                } else {
                    let db_replica_min_connections = top_config
                        .app
                        .db_replica_min_connections
                        .unwrap_or(db_min_connections);

                    let db_replica_max_connections = top_config
                        .app
                        .db_replica_max_connections
                        .unwrap_or(db_max_connections);

                    let db_replica = get_db(
                        db_replica_url,
                        db_replica_min_connections,
                        db_replica_max_connections,
                    )
                    .await?;

                    Some(DatabaseReplica(db_replica))
                }
            } else {
                // just clone so that we don't need a bunch of checks all over our code
                db_conn.clone().map(DatabaseReplica)
            };
        } else {
            if top_config.app.db_replica_url.is_some() {
                return Err(anyhow::anyhow!(
                    "if there is a db_replica_url, there must be a db_url"
                ));
            }

            warn!("no database. some features will be disabled");
        };

        let balanced_rpcs = top_config.balanced_rpcs;

        // safety check on balanced_rpcs
        if balanced_rpcs.len() < top_config.app.min_synced_rpcs {
            return Err(anyhow::anyhow!(
                "Only {}/{} rpcs! Add more balanced_rpcs or reduce min_synced_rpcs.",
                balanced_rpcs.len(),
                top_config.app.min_synced_rpcs
            ));
        }

        // safety check on sum soft limit
        let sum_soft_limit = balanced_rpcs.values().fold(0, |acc, x| acc + x.soft_limit);

        if sum_soft_limit < top_config.app.min_sum_soft_limit {
            return Err(anyhow::anyhow!(
                "Only {}/{} soft limit! Add more balanced_rpcs, increase soft limits, or reduce min_sum_soft_limit.",
                sum_soft_limit,
                top_config.app.min_sum_soft_limit
            ));
        }

        let private_rpcs = top_config.private_rpcs.unwrap_or_default();

        // these are safe to cancel
        let cancellable_handles = FuturesUnordered::new();
        // we must wait for these to end on their own (and they need to subscribe to shutdown_sender)
        let important_background_handles = FuturesUnordered::new();

        // make a http shared client
        // TODO: can we configure the connection pool? should we?
        // TODO: timeouts from config. defaults are hopefully good
        let http_client = Some(
            reqwest::ClientBuilder::new()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(60))
                .user_agent(APP_USER_AGENT)
                .build()?,
        );

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

        // setup a channel for receiving stats (generally with a high cardinality, such as per-user)
        // we do this in a channel so we don't slow down our response to the users
        let stat_sender = if let Some(db_conn) = db_conn.clone() {
            let emitter_spawn =
                StatEmitter::spawn(top_config.app.chain_id, db_conn, 60, shutdown_receiver)?;

            important_background_handles.push(emitter_spawn.background_handle);

            Some(emitter_spawn.stat_sender)
        } else {
            warn!("cannot store stats without a database connection");

            // TODO: subscribe to the shutdown_receiver here since the stat emitter isn't running?

            None
        };

        // TODO: i don't like doing Block::default here! Change this to "None"?
        let (watch_consensus_head_sender, watch_consensus_head_receiver) =
            watch::channel(Arc::new(Block::default()));
        // TODO: will one receiver lagging be okay? how big should this be?
        let (pending_tx_sender, pending_tx_receiver) = broadcast::channel(256);

        // TODO: use this? it could listen for confirmed transactions and then clear pending_transactions, but the head_block_sender is doing that
        // TODO: don't drop the pending_tx_receiver. instead, read it to mark transactions as "seen". once seen, we won't re-send them?
        // TODO: once a transaction is "Confirmed" we remove it from the map. this should prevent major memory leaks.
        // TODO: we should still have some sort of expiration or maximum size limit for the map
        drop(pending_tx_receiver);

        // TODO: capacity from configs
        // all these are the same size, so no need for a weigher
        // TODO: ttl on this? or is max_capacity fine?
        let pending_transactions = Cache::builder()
            .max_capacity(10_000)
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default());

        // keep 1GB of blocks in the cache
        // TODO: limits from config
        // these blocks don't have full transactions, but they do have rather variable amounts of transaction hashes
        // TODO: how can we do the weigher better?
        let block_map = Cache::builder()
            .max_capacity(1024 * 1024 * 1024)
            .weigher(|_k, v: &ArcBlock| {
                // TODO: is this good enough?
                1 + v.transactions.len().try_into().unwrap_or(u32::MAX)
            })
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default());

        // connect to the load balanced rpcs
        let (balanced_rpcs, balanced_handle) = Web3Connections::spawn(
            top_config.app.chain_id,
            db_conn.clone(),
            balanced_rpcs,
            http_client.clone(),
            vredis_pool.clone(),
            block_map.clone(),
            Some(watch_consensus_head_sender),
            top_config.app.min_sum_soft_limit,
            top_config.app.min_synced_rpcs,
            Some(pending_tx_sender.clone()),
            pending_transactions.clone(),
            open_request_handle_metrics.clone(),
        )
        .await
        .context("spawning balanced rpcs")?;

        // save the handle to catch any errors
        cancellable_handles.push(balanced_handle);

        // connect to the private rpcs
        // only some chains have this, so this is optional
        let private_rpcs = if private_rpcs.is_empty() {
            // TODO: do None instead of clone?
            warn!("No private relays configured. Any transactions will be broadcast to the public mempool!");
            None
        } else {
            let (private_rpcs, private_handle) = Web3Connections::spawn(
                top_config.app.chain_id,
                db_conn.clone(),
                private_rpcs,
                http_client.clone(),
                vredis_pool.clone(),
                block_map,
                // subscribing to new heads here won't work well. if they are fast, they might be ahead of balanced_rpcs
                // they also often have low rate limits
                // however, they are well connected to miners/validators. so maybe using them as a safety check would be good
                None,
                0,
                0,
                // TODO: subscribe to pending transactions on the private rpcs? they seem to have low rate limits
                None,
                pending_transactions.clone(),
                open_request_handle_metrics.clone(),
            )
            .await
            .context("spawning private_rpcs")?;

            if private_rpcs.conns.is_empty() {
                None
            } else {
                // save the handle to catch any errors
                cancellable_handles.push(private_handle);

                Some(private_rpcs)
            }
        };

        // create rate limiters
        // these are optional. they require redis
        let mut frontend_ip_rate_limiter = None;
        let mut frontend_registered_user_rate_limiter = None;
        let mut login_rate_limiter = None;

        if let Some(redis_pool) = vredis_pool.as_ref() {
            let rpc_rrl = RedisRateLimiter::new(
                "web3_proxy",
                "frontend",
                // TODO: think about this unwrapping
                top_config
                    .app
                    .public_requests_per_period
                    .unwrap_or(u64::MAX),
                60.0,
                redis_pool.clone(),
            );

            // these two rate limiters can share the base limiter
            // these are deferred rate limiters because we don't want redis network requests on the hot path
            // TODO: take cache_size from config
            frontend_ip_rate_limiter = Some(DeferredRateLimiter::<IpAddr>::new(
                10_000,
                "ip",
                rpc_rrl.clone(),
                None,
            ));
            frontend_registered_user_rate_limiter = Some(DeferredRateLimiter::<u64>::new(
                10_000, "key", rpc_rrl, None,
            ));

            login_rate_limiter = Some(RedisRateLimiter::new(
                "web3_proxy",
                "login",
                top_config.app.login_rate_limit_per_period,
                60.0,
                redis_pool.clone(),
            ));
        }

        // keep 1GB of blocks in the cache
        // responses can be very different in sizes, so this definitely needs a weigher
        // TODO: max_capacity from config
        // TODO: don't allow any response to be bigger than X% of the cache
        let response_cache = Cache::builder()
            .max_capacity(1024 * 1024 * 1024)
            .weigher(|k: &ResponseCacheKey, v| {
                // TODO: is this good?
                if let Ok(v) = serde_json::to_string(v) {
                    let weight = k.weight() + v.len();

                    // the or in unwrap_or is probably never called
                    weight.try_into().unwrap_or(u32::MAX)
                } else {
                    // this seems impossible
                    u32::MAX
                }
            })
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default());

        // all the users are the same size, so no need for a weigher
        // if there is no database of users, there will be no keys and so this will be empty
        // TODO: max_capacity from config
        // TODO: ttl from config
        let rpc_secret_key_cache = Cache::builder()
            .max_capacity(10_000)
            .time_to_live(Duration::from_secs(600))
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default());

        // create semaphores for concurrent connection limits
        // TODO: what should tti be for semaphores?
        let bearer_token_semaphores = Cache::builder()
            .time_to_idle(Duration::from_secs(120))
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default());
        let ip_semaphores = Cache::builder()
            .time_to_idle(Duration::from_secs(120))
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default());
        let registered_user_semaphores = Cache::builder()
            .time_to_idle(Duration::from_secs(120))
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default());

        let app = Self {
            config: top_config.app,
            balanced_rpcs,
            private_rpcs,
            response_cache,
            watch_consensus_head_receiver,
            pending_tx_sender,
            pending_transactions,
            frontend_ip_rate_limiter,
            frontend_registered_user_rate_limiter,
            login_rate_limiter,
            db_conn,
            db_replica,
            vredis_pool,
            app_metrics,
            open_request_handle_metrics,
            rpc_secret_key_cache,
            bearer_token_semaphores,
            ip_semaphores,
            registered_user_semaphores,
            stat_sender,
        };

        let app = Arc::new(app);

        Ok((app, cancellable_handles, important_background_handles).into())
    }

    #[instrument(level = "trace")]
    pub fn head_block_receiver(&self) -> watch::Receiver<ArcBlock> {
        self.watch_consensus_head_receiver.clone()
    }

    #[instrument(level = "trace")]
    pub async fn prometheus_metrics(&self) -> String {
        let globals = HashMap::new();
        // TODO: what globals? should this be the hostname or what?
        // globals.insert("service", "web3_proxy");

        #[derive(Default, Serialize)]
        struct UserCount(i64);

        let user_count: UserCount = if let Some(db) = self.db_conn() {
            match user::Entity::find().count(&db).await {
                Ok(user_count) => UserCount(user_count as i64),
                Err(err) => {
                    warn!("unable to count users: {:?}", err);
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
            Ok(Some(mut redis_conn)) => {
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
                        warn!("unable to count recent users: {}", err);
                        (
                            RecentCounts::for_err(),
                            RecentCounts::for_err(),
                            RecentCounts::for_err(),
                        )
                    }
                }
            }
            Ok(None) => (
                RecentCounts::default(),
                RecentCounts::default(),
                RecentCounts::default(),
            ),
            Err(err) => {
                warn!("unable to connect to redis while counting users: {:?}", err);
                (
                    RecentCounts::for_err(),
                    RecentCounts::for_err(),
                    RecentCounts::for_err(),
                )
            }
        };

        // app.pending_transactions.sync();
        // app.rpc_secret_key_cache.sync();
        // "pending_transactions_count": app.pending_transactions.entry_count(),
        // "pending_transactions_size": app.pending_transactions.weighted_size(),
        // "user_cache_count": app.rpc_secret_key_cache.entry_count(),
        // "user_cache_size": app.rpc_secret_key_cache.weighted_size(),

        #[derive(Serialize)]
        struct CombinedMetrics<'a> {
            app: &'a Web3ProxyAppMetrics,
            backend_rpc: &'a OpenRequestHandleMetrics,
            recent_ip_counts: RecentCounts,
            recent_user_id_counts: RecentCounts,
            recent_tx_counts: RecentCounts,
            user_count: UserCount,
        }

        let metrics = CombinedMetrics {
            app: &self.app_metrics,
            backend_rpc: &self.open_request_handle_metrics,
            recent_ip_counts,
            recent_user_id_counts,
            recent_tx_counts,
            user_count,
        };

        serde_prometheus::to_string(&metrics, Some("web3_proxy"), globals)
            .expect("prometheus metrics should always serialize")
    }

    /// send the request or batch of requests to the approriate RPCs
    #[instrument(level = "trace")]
    pub async fn proxy_web3_rpc(
        self: &Arc<Self>,
        authorization: Arc<Authorization>,
        request: JsonRpcRequestEnum,
        proxy_mode: ProxyMode,
    ) -> Result<(JsonRpcForwardedResponseEnum, Vec<Arc<Web3Connection>>), FrontendErrorResponse>
    {
        // trace!(?request, "proxy_web3_rpc");

        // even though we have timeouts on the requests to our backend providers,
        // we need a timeout for the incoming request so that retries don't run forever
        // TODO: take this as an optional argument. per user max? expiration time instead of duration?
        let max_time = Duration::from_secs(120);

        let response = match request {
            JsonRpcRequestEnum::Single(request) => {
                let (response, rpcs) = timeout(
                    max_time,
                    self.proxy_cached_request(&authorization, request, proxy_mode),
                )
                .await??;

                (JsonRpcForwardedResponseEnum::Single(response), rpcs)
            }
            JsonRpcRequestEnum::Batch(requests) => {
                let (responses, rpcs) = timeout(
                    max_time,
                    self.proxy_web3_rpc_requests(&authorization, requests, proxy_mode),
                )
                .await??;

                (JsonRpcForwardedResponseEnum::Batch(responses), rpcs)
            }
        };

        Ok(response)
    }

    /// cut up the request and send to potentually different servers
    /// TODO: make sure this isn't a problem
    #[instrument(level = "trace")]
    async fn proxy_web3_rpc_requests(
        self: &Arc<Self>,
        authorization: &Arc<Authorization>,
        requests: Vec<JsonRpcRequest>,
        proxy_mode: ProxyMode,
    ) -> anyhow::Result<(Vec<JsonRpcForwardedResponse>, Vec<Arc<Web3Connection>>)> {
        // TODO: we should probably change ethers-rs to support this directly. they pushed this off to v2 though
        let num_requests = requests.len();

        // TODO: spawn so the requests go in parallel? need to think about rate limiting more if we do that
        // TODO: improve flattening
        let responses = join_all(
            requests
                .into_iter()
                .map(|request| self.proxy_cached_request(authorization, request, proxy_mode))
                .collect::<Vec<_>>(),
        )
        .await;

        // TODO: i'm sure this could be done better with iterators
        // TODO: stream the response?
        let mut collected: Vec<JsonRpcForwardedResponse> = Vec::with_capacity(num_requests);
        let mut collected_rpcs: HashSet<Arc<Web3Connection>> = HashSet::new();
        for response in responses {
            // TODO: any way to attach the tried rpcs to the error? it is likely helpful
            let (response, rpcs) = response?;

            collected.push(response);
            collected_rpcs.extend(rpcs.into_iter());
        }

        let collected_rpcs: Vec<_> = collected_rpcs.into_iter().collect();

        Ok((collected, collected_rpcs))
    }

    /// TODO: i don't think we want or need this. just use app.db_conn, or maybe app.db_conn.clone() or app.db_conn.as_ref()
    #[instrument(level = "trace")]
    pub fn db_conn(&self) -> Option<DatabaseConnection> {
        self.db_conn.clone()
    }

    #[instrument(level = "trace")]
    pub fn db_replica(&self) -> Option<DatabaseReplica> {
        self.db_replica.clone()
    }

    #[instrument(level = "trace")]
    pub async fn redis_conn(&self) -> anyhow::Result<Option<redis_rate_limiter::RedisConnection>> {
        match self.vredis_pool.as_ref() {
            // TODO: don't do an error. return None
            None => Ok(None),
            Some(redis_pool) => {
                let redis_conn = redis_pool.get().await?;

                Ok(Some(redis_conn))
            }
        }
    }

    #[measure([ErrorCount, HitCount, ResponseTime, Throughput])]
    #[instrument(level = "trace")]
    async fn proxy_cached_request(
        self: &Arc<Self>,
        authorization: &Arc<Authorization>,
        mut request: JsonRpcRequest,
        proxy_mode: ProxyMode,
    ) -> anyhow::Result<(JsonRpcForwardedResponse, Vec<Arc<Web3Connection>>)> {
        // trace!("Received request: {:?}", request);

        let request_metadata = Arc::new(RequestMetadata::new(REQUEST_PERIOD, request.num_bytes())?);

        // save the id so we can attach it to the response
        // TODO: instead of cloning, take the id out?
        let request_id = request.id.clone();
        let request_method = request.method.clone();

        // TODO: if eth_chainId or net_version, serve those without querying the backend
        // TODO: don't clone?
        let partial_response: serde_json::Value = match request_method.as_ref() {
            // lots of commands are blocked
            method @ ("admin_addPeer"
            | "admin_datadir"
            | "admin_startRPC"
            | "admin_startWS"
            | "admin_stopRPC"
            | "admin_stopWS"
            | "db_getHex"
            | "db_getString"
            | "db_putHex"
            | "db_putString"
            | "debug_chaindbCompact"
            | "debug_freezeClient"
            | "debug_goTrace"
            | "debug_mutexProfile"
            | "debug_setBlockProfileRate"
            | "debug_setGCPercent"
            | "debug_setHead"
            | "debug_setMutexProfileFraction"
            | "debug_standardTraceBlockToFile"
            | "debug_standardTraceBadBlockToFile"
            | "debug_startCPUProfile"
            | "debug_startGoTrace"
            | "debug_stopCPUProfile"
            | "debug_stopGoTrace"
            | "debug_writeBlockProfile"
            | "debug_writeMemProfile"
            | "debug_writeMutexProfile"
            | "eth_compileLLL"
            | "eth_compileSerpent"
            | "eth_compileSolidity"
            | "eth_getCompilers"
            | "eth_sendTransaction"
            | "eth_sign"
            | "eth_signTransaction"
            | "eth_submitHashrate"
            | "eth_submitWork"
            | "erigon_cacheCheck"
            | "les_addBalance"
            | "les_setClientParams"
            | "les_setDefaultParams"
            | "miner_setExtra"
            | "miner_setGasPrice"
            | "miner_start"
            | "miner_stop"
            | "miner_setEtherbase"
            | "miner_setGasLimit"
            | "personal_importRawKey"
            | "personal_listAccounts"
            | "personal_lockAccount"
            | "personal_newAccount"
            | "personal_unlockAccount"
            | "personal_sendTransaction"
            | "personal_sign"
            | "personal_ecRecover"
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
                // TODO: client error stat
                // TODO: what error code?
                return Ok((
                    JsonRpcForwardedResponse::from_string(
                        format!("method unsupported: {}", method),
                        None,
                        Some(request_id),
                    ),
                    vec![],
                ));
            }
            // TODO: implement these commands
            method @ ("eth_getFilterChanges"
            | "eth_getFilterLogs"
            | "eth_newBlockFilter"
            | "eth_newFilter"
            | "eth_newPendingTransactionFilter"
            | "eth_uninstallFilter") => {
                // TODO: unsupported command stat
                // TODO: what error code?
                return Ok((
                    JsonRpcForwardedResponse::from_string(
                        format!("not yet implemented: {}", method),
                        None,
                        Some(request_id),
                    ),
                    vec![],
                ));
            }
            // some commands can use local data or caches
            "eth_accounts" => {
                // no stats on this. its cheap
                serde_json::Value::Array(vec![])
            }
            "eth_blockNumber" => {
                match self.balanced_rpcs.head_block_num() {
                    Some(head_block_num) => {
                        json!(head_block_num)
                    }
                    None => {
                        // TODO: what does geth do if this happens?
                        return Err(anyhow::anyhow!(
                            "no servers synced. unknown eth_blockNumber"
                        ));
                    }
                }
            }
            "eth_chainId" => {
                json!(U64::from(self.config.chain_id))
            }
            // TODO: eth_callBundle (https://docs.flashbots.net/flashbots-auction/searchers/advanced/rpc-endpoint#eth_callbundle)
            // TODO: eth_cancelPrivateTransaction (https://docs.flashbots.net/flashbots-auction/searchers/advanced/rpc-endpoint#eth_cancelprivatetransaction, but maybe just reject)
            // TODO: eth_sendPrivateTransaction (https://docs.flashbots.net/flashbots-auction/searchers/advanced/rpc-endpoint#eth_sendprivatetransaction)
            "eth_coinbase" => {
                // no need for serving coinbase
                // no stats on this. its cheap
                json!(Address::zero())
            }
            "eth_estimateGas" => {
                let mut response = self
                    .balanced_rpcs
                    .try_proxy_connection(
                        proxy_mode,
                        authorization,
                        request,
                        Some(&request_metadata),
                        None,
                    )
                    .await?;

                let mut gas_estimate: U256 = if let Some(gas_estimate) = response.result.take() {
                    serde_json::from_str(gas_estimate.get())
                        .context("gas estimate result is not an U256")?
                } else {
                    // i think this is always an error response
                    let rpcs = request_metadata.backend_requests.lock().clone();

                    return Ok((response, rpcs));
                };

                let gas_increase =
                    if let Some(gas_increase_percent) = self.config.gas_increase_percent {
                        let gas_increase = gas_estimate * gas_increase_percent / U256::from(100);

                        let min_gas_increase = self.config.gas_increase_min.unwrap_or_default();

                        gas_increase.max(min_gas_increase)
                    } else {
                        self.config.gas_increase_min.unwrap_or_default()
                    };

                gas_estimate += gas_increase;

                json!(gas_estimate)
            }
            // TODO: eth_gasPrice that does awesome magic to predict the future
            "eth_hashrate" => {
                // no stats on this. its cheap
                json!(U64::zero())
            }
            "eth_mining" => {
                // no stats on this. its cheap
                json!(false)
            }
            // TODO: eth_sendBundle (flashbots command)
            // broadcast transactions to all private rpcs at once
            "eth_sendRawTransaction" => {
                // TODO: how should we handle private_mode here?
                let default_num = match proxy_mode {
                    // TODO: how many balanced rpcs should we send to? configurable? percentage of total?
                    ProxyMode::Best => Some(4),
                    ProxyMode::Fastest(0) => None,
                    // TODO: how many balanced rpcs should we send to? configurable? percentage of total?
                    // TODO: what if we do 2 per tier? we want to blast the third party rpcs
                    // TODO: maybe having the third party rpcs in their own Web3Connections would be good for this
                    ProxyMode::Fastest(x) => Some(x * 4),
                    ProxyMode::Versus => None,
                };

                let (private_rpcs, num) = if let Some(private_rpcs) = self.private_rpcs.as_ref() {
                    if authorization.checks.private_txs {
                        // if we are sending the transaction privately, no matter the proxy_mode, we send to ALL private rpcs
                        (private_rpcs, None)
                    } else {
                        (&self.balanced_rpcs, default_num)
                    }
                } else {
                    (&self.balanced_rpcs, default_num)
                };

                // try_send_all_upstream_servers puts the request id into the response. no need to do that ourselves here.
                let mut response = private_rpcs
                    .try_send_all_synced_connections(
                        authorization,
                        &request,
                        Some(request_metadata.clone()),
                        None,
                        Level::Trace,
                        num,
                    )
                    .await?;

                // sometimes we get an error that the transaction is already known by our nodes,
                // that's not really an error. Just return the hash like a successful response would.
                if let Some(response_error) = response.error.as_ref() {
                    if response_error.code == -32000
                        && (response_error.message == "ALREADY_EXISTS: already known"
                            || response_error.message
                                == "INTERNAL_ERROR: existing tx with same hash")
                    {
                        let params = request
                            .params
                            .context("there must be params if we got this far")?;

                        let params = params
                            .as_array()
                            .context("there must be an array if we got this far")?
                            .get(0)
                            .context("there must be an item if we got this far")?
                            .as_str()
                            .context("there must be a string if we got this far")?;

                        let params = Bytes::from_str(params)
                            .expect("there must be Bytes if we got this far");

                        let rlp = Rlp::new(params.as_ref());

                        if let Ok(tx) = Transaction::decode(&rlp) {
                            let tx_hash = json!(tx.hash());

                            trace!("tx_hash: {:#?}", tx_hash);

                            let tx_hash = to_raw_value(&tx_hash).unwrap();

                            response.error = None;
                            response.result = Some(tx_hash);
                        }
                    }
                }

                let rpcs = request_metadata.backend_requests.lock().clone();

                // emit stats
                if let Some(salt) = self.config.public_recent_ips_salt.as_ref() {
                    if let Some(tx_hash) = response.result.clone() {
                        let now = Utc::now().timestamp();
                        let salt = salt.clone();
                        let app = self.clone();

                        let f = async move {
                            match app.redis_conn().await {
                                Ok(Some(mut redis_conn)) => {
                                    let salted_tx_hash = format!("{}:{}", salt, tx_hash);

                                    let hashed_tx_hash =
                                        Bytes::from(keccak256(salted_tx_hash.as_bytes()));

                                    let recent_tx_hash_key =
                                        format!("eth_sendRawTransaction:{}", app.config.chain_id);

                                    redis_conn
                                        .zadd(recent_tx_hash_key, hashed_tx_hash.to_string(), now)
                                        .await?;
                                }
                                Ok(None) => {}
                                Err(err) => {
                                    warn!(
                                        "unable to save stats for eth_sendRawTransaction: {:?}",
                                        err
                                    )
                                }
                            }

                            Ok::<_, anyhow::Error>(())
                        };

                        tokio::spawn(f);
                    }
                }

                return Ok((response, rpcs));
            }
            "eth_syncing" => {
                // no stats on this. its cheap
                // TODO: return a real response if all backends are syncing or if no servers in sync
                json!(false)
            }
            "eth_subscribe" => {
                return Ok((
                    JsonRpcForwardedResponse::from_str(
                        "notifications not supported. eth_subscribe is only available over a websocket",
                        Some(-32601),
                        Some(request_id),
                    ),
                    vec![],
                ));
            }
            "eth_unsubscribe" => {
                return Ok((
                    JsonRpcForwardedResponse::from_str(
                        "notifications not supported. eth_unsubscribe is only available over a websocket",
                        Some(-32601),
                        Some(request_id),
                    ),
                    vec![],
                ));
            }
            "net_listening" => {
                // no stats on this. its cheap
                // TODO: only if there are some backends on balanced_rpcs?
                json!(true)
            }
            "net_peerCount" => {
                // no stats on this. its cheap
                // TODO: do something with proxy_mode here?
                self.balanced_rpcs.num_synced_rpcs().into()
            }
            "web3_clientVersion" => {
                // no stats on this. its cheap
                serde_json::Value::String(APP_USER_AGENT.to_string())
            }
            "web3_sha3" => {
                // emit stats
                // returns Keccak-256 (not the standardized SHA3-256) of the given data.
                match &request.params {
                    Some(serde_json::Value::Array(params)) => {
                        // TODO: make a struct and use serde conversion to clean this up
                        if params.len() != 1 || !params[0].is_string() {
                            // TODO: what error code?
                            return Ok((
                                JsonRpcForwardedResponse::from_str(
                                    "Invalid request",
                                    Some(-32600),
                                    Some(request_id),
                                ),
                                vec![],
                            ));
                        }

                        // TODO: don't return with ? here. send a jsonrpc invalid request
                        let param = Bytes::from_str(
                            params[0]
                                .as_str()
                                .context("parsing params 0 into str then bytes")?,
                        )?;

                        let hash = H256::from(keccak256(param));

                        json!(hash)
                    }
                    _ => {
                        // TODO: this needs the correct error code in the response
                        // TODO: emit stat?
                        return Ok((
                            JsonRpcForwardedResponse::from_str(
                                "invalid request",
                                None,
                                Some(request_id),
                            ),
                            vec![],
                        ));
                    }
                }
            }
            "test" => {
                return Ok((
                    JsonRpcForwardedResponse::from_str(
                        "The method test does not exist/is not available.",
                        Some(-32601),
                        Some(request_id),
                    ),
                    vec![],
                ));
            }
            // anything else gets sent to backend rpcs and cached
            method => {
                // emit stats

                // TODO: if no servers synced, wait for them to be synced? probably better to error and let haproxy retry another server
                let head_block_num = self
                    .balanced_rpcs
                    .head_block_num()
                    .context("no servers synced")?;

                // we do this check before checking caches because it might modify the request params
                // TODO: add a stat for archive vs full since they should probably cost different
                // TODO: this cache key can be rather large. is that okay?
                let cache_key: Option<ResponseCacheKey> = match block_needed(
                    authorization,
                    method,
                    request.params.as_mut(),
                    head_block_num,
                    &self.balanced_rpcs,
                )
                .await?
                {
                    BlockNeeded::CacheSuccessForever => Some(ResponseCacheKey {
                        block: None,
                        method: method.to_string(),
                        params: request.params.clone(),
                        cache_errors: false,
                    }),
                    BlockNeeded::CacheNever => None,
                    BlockNeeded::Cache {
                        block_num,
                        cache_errors,
                    } => {
                        let (request_block_hash, archive_needed) = self
                            .balanced_rpcs
                            .block_hash(authorization, &block_num)
                            .await?;

                        if archive_needed {
                            request_metadata
                                .archive_request
                                .store(true, atomic::Ordering::Relaxed);
                        }

                        let request_block = self
                            .balanced_rpcs
                            .block(authorization, &request_block_hash, None)
                            .await?;

                        Some(ResponseCacheKey {
                            block: Some(SavedBlock::new(request_block)),
                            method: method.to_string(),
                            // TODO: hash here?
                            params: request.params.clone(),
                            cache_errors,
                        })
                    }
                };

                let mut response = {
                    let request_metadata = request_metadata.clone();

                    let authorization = authorization.clone();

                    if let Some(cache_key) = cache_key {
                        let request_block_number = cache_key.block.as_ref().map(|x| x.number());

                        self.response_cache
                            .try_get_with(cache_key, async move {
                                // TODO: retry some failures automatically!
                                // TODO: try private_rpcs if all the balanced_rpcs fail!
                                // TODO: put the hash here instead of the block number? its in the request already.

                                let mut response = self
                                    .balanced_rpcs
                                    .try_proxy_connection(
                                        proxy_mode,
                                        &authorization,
                                        request,
                                        Some(&request_metadata),
                                        request_block_number.as_ref(),
                                    )
                                    .await?;

                                // discard their id by replacing it with an empty
                                response.id = Default::default();

                                // TODO: only cache the inner response
                                Ok::<_, anyhow::Error>(response)
                            })
                            .await
                            // TODO: what is the best way to handle an Arc here?
                            .map_err(|err| {
                                // TODO: emit a stat for an error
                                anyhow::anyhow!(
                                    "error while caching and forwarding response: {}",
                                    err
                                )
                            })?
                    } else {
                        self.balanced_rpcs
                            .try_proxy_connection(
                                proxy_mode,
                                &authorization,
                                request,
                                Some(&request_metadata),
                                None,
                            )
                            .await?
                    }
                };

                // since this data came likely out of a cache, the id is not going to match
                // replace the id with our request's id.
                response.id = request_id;

                // TODO: DRY!
                let rpcs = request_metadata.backend_requests.lock().clone();

                if let Some(stat_sender) = self.stat_sender.as_ref() {
                    let response_stat = ProxyResponseStat::new(
                        method.to_string(),
                        authorization.clone(),
                        request_metadata,
                        response.num_bytes(),
                    );

                    stat_sender
                        .send_async(response_stat.into())
                        .await
                        .context("stat_sender sending response_stat")?;
                }

                return Ok((response, rpcs));
            }
        };

        let response = JsonRpcForwardedResponse::from_value(partial_response, request_id);

        // TODO: DRY
        let rpcs = request_metadata.backend_requests.lock().clone();

        if let Some(stat_sender) = self.stat_sender.as_ref() {
            let response_stat = ProxyResponseStat::new(
                request_method,
                authorization.clone(),
                request_metadata,
                response.num_bytes(),
            );

            stat_sender
                .send_async(response_stat.into())
                .await
                .context("stat_sender sending response stat")?;
        }

        Ok((response, rpcs))
    }
}

impl fmt::Debug for Web3ProxyApp {
    #[instrument(skip_all)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("Web3ProxyApp").finish_non_exhaustive()
    }
}
