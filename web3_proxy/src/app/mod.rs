// TODO: this file is way too big now. move things into other modules
mod ws;

use crate::block_number::{block_needed, BlockNeeded};
use crate::config::{AppConfig, TopConfig};
use crate::frontend::authorization::{Authorization, RequestMetadata, RpcSecretKey};
use crate::frontend::errors::FrontendErrorResponse;
use crate::frontend::rpc_proxy_ws::ProxyMode;
use crate::jsonrpc::{
    JsonRpcForwardedResponse, JsonRpcForwardedResponseEnum, JsonRpcRequest, JsonRpcRequestEnum,
};
use crate::rpcs::blockchain::{BlockHashesCache, Web3ProxyBlock};
use crate::rpcs::consensus::ConsensusWeb3Rpcs;
use crate::rpcs::many::Web3Rpcs;
use crate::rpcs::one::Web3Rpc;
use crate::rpcs::transactions::TxStatus;
use crate::stats::{AppStat, RpcQueryStats, StatBuffer};
use crate::user_token::UserBearerToken;
use anyhow::Context;
use axum::headers::{Origin, Referer, UserAgent};
use chrono::Utc;
use deferred_rate_limiter::DeferredRateLimiter;
use derive_more::From;
use entities::sea_orm_active_enums::TrackingLevel;
use entities::user;
use ethers::core::utils::keccak256;
use ethers::prelude::{Address, Bytes, Transaction, TxHash, H256, U64};
use ethers::types::U256;
use ethers::utils::rlp::{Decodable, Rlp};
use futures::future::join_all;
use futures::stream::{FuturesUnordered, StreamExt};
use hashbrown::{HashMap, HashSet};
use ipnet::IpNet;
use log::{debug, error, info, trace, warn, Level};
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

// aggregate across 1 week
const BILLING_PERIOD_SECONDS: i64 = 60 * 60 * 24 * 7;

#[derive(From)]
struct ResponseCacheKey {
    // if none, this is cached until evicted
    from_block: Option<Web3ProxyBlock>,
    // to_block is only set when ranges of blocks are requested (like with eth_getLogs)
    to_block: Option<Web3ProxyBlock>,
    method: String,
    // TODO: better type for this
    params: Option<serde_json::Value>,
    cache_errors: bool,
}

impl ResponseCacheKey {
    fn weight(&self) -> usize {
        let mut w = self.method.len();

        if let Some(p) = self.params.as_ref() {
            w += p.to_string().len();
        }

        w
    }
}

impl PartialEq for ResponseCacheKey {
    fn eq(&self, other: &Self) -> bool {
        if self.cache_errors != other.cache_errors {
            return false;
        }

        match (self.from_block.as_ref(), other.from_block.as_ref()) {
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

        match (self.to_block.as_ref(), other.to_block.as_ref()) {
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
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.from_block.as_ref().map(|x| x.hash()).hash(state);
        self.to_block.as_ref().map(|x| x.hash()).hash(state);
        self.method.hash(state);
        self.params.as_ref().map(|x| x.to_string()).hash(state);
        self.cache_errors.hash(state)
    }
}

type ResponseCache =
    Cache<ResponseCacheKey, JsonRpcForwardedResponse, hashbrown::hash_map::DefaultHashBuilder>;

pub type AnyhowJoinHandle<T> = JoinHandle<anyhow::Result<T>>;

/// TODO: move this
#[derive(Clone, Debug, Default, From)]
pub struct AuthorizationChecks {
    /// database id of the primary user. 0 if anon
    /// TODO: do we need this? its on the authorization so probably not
    /// TODO: Option<NonZeroU64>?
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
    /// how detailed any rpc account entries should be
    pub tracking_level: TrackingLevel,
    /// Chance to save reverting eth_call, eth_estimateGas, and eth_sendRawTransaction to the database.
    /// depending on the caller, errors might be expected. this keeps us from bloating our database
    /// TODO: f32 would be fine
    pub log_revert_chance: f64,
    /// if true, transactions are broadcast only to private mempools.
    /// IMPORTANT! Once confirmed by a miner, they will be public on the blockchain!
    pub private_txs: bool,
}

/// Simple wrapper so that we can keep track of read only connections.
/// This does no blocking of writing in the compiler!
/// TODO: move this
#[derive(Clone)]
pub struct DatabaseReplica(pub DatabaseConnection);

// TODO: I feel like we could do something smart with DeRef or AsRef or Borrow, but that wasn't working for me
impl DatabaseReplica {
    pub fn conn(&self) -> &DatabaseConnection {
        &self.0
    }
}

/// The application
// TODO: i'm sure this is more arcs than necessary, but spawning futures makes references hard
pub struct Web3ProxyApp {
    /// Send requests to the best server available
    pub balanced_rpcs: Arc<Web3Rpcs>,
    /// application config
    /// TODO: this will need a large refactor to handle reloads while running. maybe use a watch::Receiver?
    pub config: AppConfig,
    /// Send private requests (like eth_sendRawTransaction) to all these servers
    /// TODO: include another type so that we can use private miner relays that do not use JSONRPC requests
    pub private_rpcs: Option<Arc<Web3Rpcs>>,
    /// track JSONRPC responses
    response_cache: ResponseCache,
    /// rpc clients that subscribe to newHeads use this channel
    /// don't drop this or the sender will stop working
    /// TODO: broadcast channel instead?
    pub watch_consensus_head_receiver: watch::Receiver<Option<Web3ProxyBlock>>,
    /// rpc clients that subscribe to pendingTransactions use this channel
    /// This is the Sender so that new channels can subscribe to it
    pending_tx_sender: broadcast::Sender<TxStatus>,
    /// Optional database for users and accounting
    pub db_conn: Option<sea_orm::DatabaseConnection>,
    /// Optional read-only database for users and accounting
    pub db_replica: Option<DatabaseReplica>,
    /// store pending transactions that we've seen so that we don't send duplicates to subscribers
    /// TODO: think about this more. might be worth storing if we sent the transaction or not and using this for automatic retries
    pub pending_transactions: Cache<TxHash, TxStatus, hashbrown::hash_map::DefaultHashBuilder>,
    /// rate limit anonymous users
    pub frontend_ip_rate_limiter: Option<DeferredRateLimiter<IpAddr>>,
    /// rate limit authenticated users
    pub frontend_registered_user_rate_limiter: Option<DeferredRateLimiter<u64>>,
    /// Optional time series database for making pretty graphs that load quickly
    pub influxdb_client: Option<influxdb2::Client>,
    /// rate limit the login endpoint
    /// we do this because each pending login is a row in the database
    pub login_rate_limiter: Option<RedisRateLimiter>,
    /// volatile cache used for rate limits
    /// TODO: i think i might just delete this entirely. instead use local-only concurrency limits.
    pub vredis_pool: Option<RedisPool>,
    /// cache authenticated users so that we don't have to query the database on the hot path
    // TODO: should the key be our RpcSecretKey class instead of Ulid?
    pub rpc_secret_key_cache:
        Cache<Ulid, AuthorizationChecks, hashbrown::hash_map::DefaultHashBuilder>,
    /// concurrent/parallel RPC request limits for authenticated users
    pub registered_user_semaphores:
        Cache<NonZeroU64, Arc<Semaphore>, hashbrown::hash_map::DefaultHashBuilder>,
    /// concurrent/parallel request limits for anonymous users
    pub ip_semaphores: Cache<IpAddr, Arc<Semaphore>, hashbrown::hash_map::DefaultHashBuilder>,
    /// concurrent/parallel application request limits for authenticated users
    pub bearer_token_semaphores:
        Cache<UserBearerToken, Arc<Semaphore>, hashbrown::hash_map::DefaultHashBuilder>,
    /// channel for sending stats in a background task
    pub stat_sender: Option<flume::Sender<AppStat>>,
}

/// flatten a JoinError into an anyhow error
/// Useful when joining multiple futures.
pub async fn flatten_handle<T>(handle: AnyhowJoinHandle<T>) -> anyhow::Result<T> {
    match handle.await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(err)) => Err(err),
        Err(err) => Err(err.into()),
    }
}

/// return the first error or okay if everything worked

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

pub async fn drop_migration_lock(db_conn: &DatabaseConnection) -> Result<(), DbErr> {
    let db_backend = db_conn.get_database_backend();

    let drop_lock_statment = db_backend.build(Table::drop().table(Alias::new("migration_lock")));

    db_conn.execute(drop_lock_statment).await?;

    debug!("migration lock unlocked");

    Ok(())
}

/// Be super careful with override_existing_lock! It is very important that only one process is running the migrations at a time!
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
        if Migrator::get_pending_migrations(db_conn).await?.is_empty() {
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

    let migration_result = Migrator::up(db_conn, None).await;

    // drop the distributed lock
    drop_migration_lock(db_conn).await?;

    // return if migrations erred
    migration_result
}

/// Connect to the database and run migrations
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

/// starting an app creates many tasks
#[derive(From)]
pub struct Web3ProxyAppSpawn {
    /// the app. probably clone this to use in other groups of handles
    pub app: Arc<Web3ProxyApp>,
    // cancellable handles
    pub app_handles: FuturesUnordered<AnyhowJoinHandle<()>>,
    /// these are important and must be allowed to finish
    pub background_handles: FuturesUnordered<AnyhowJoinHandle<()>>,
    /// watch this to know when to start the app
    pub consensus_connections_watcher: watch::Receiver<Arc<ConsensusWeb3Rpcs>>,
}

impl Web3ProxyApp {
    /// The main entrypoint.
    pub async fn spawn(
        top_config: TopConfig,
        num_workers: usize,
        shutdown_sender: broadcast::Sender<()>,
    ) -> anyhow::Result<Web3ProxyAppSpawn> {
        let rpc_account_shutdown_recevier = shutdown_sender.subscribe();
        let mut background_shutdown_receiver = shutdown_sender.subscribe();

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
                .timeout(Duration::from_secs(5 * 60))
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

                let influxdb_client =
                    influxdb2::Client::new(influxdb_host, influxdb_org, influxdb_token);

                // TODO: test the client now. having a stat for "started" can be useful on graphs to mark deploys

                Some(influxdb_client)
            }
            None => None,
        };

        // create a channel for receiving stats
        // we do this in a channel so we don't slow down our response to the users
        // stats can be saved in mysql, influxdb, both, or none
        let stat_sender = if let Some(emitter_spawn) = StatBuffer::try_spawn(
            top_config.app.chain_id,
            db_conn.clone(),
            influxdb_client.clone(),
            60,
            1,
            BILLING_PERIOD_SECONDS,
            rpc_account_shutdown_recevier,
        )? {
            // since the database entries are used for accounting, we want to be sure everything is saved before exiting
            important_background_handles.push(emitter_spawn.background_handle);

            Some(emitter_spawn.stat_sender)
        } else {
            None
        };

        // TODO: i don't like doing Block::default here! Change this to "None"?
        let (watch_consensus_head_sender, watch_consensus_head_receiver) = watch::channel(None);
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
            // TODO: different chains might handle this differently
            // TODO: what should we set? 5 minutes is arbitrary. the nodes themselves hold onto transactions for much longer
            .time_to_idle(Duration::from_secs(300))
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default());

        // keep 1GB/5 minutes of blocks in the cache
        // TODO: limits from config
        // these blocks don't have full transactions, but they do have rather variable amounts of transaction hashes
        // TODO: how can we do the weigher better?
        let block_map: BlockHashesCache = Cache::builder()
            .max_capacity(1024 * 1024 * 1024)
            .weigher(|_k, v: &Web3ProxyBlock| {
                // TODO: is this good enough?
                1 + v.block.transactions.len().try_into().unwrap_or(u32::MAX)
            })
            // TODO: what should we set? 5 minutes is arbitrary. the nodes themselves hold onto transactions for much longer
            .time_to_idle(Duration::from_secs(300))
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default());

        // connect to the load balanced rpcs
        let (balanced_rpcs, balanced_handle, consensus_connections_watcher) = Web3Rpcs::spawn(
            block_map.clone(),
            top_config.app.chain_id,
            db_conn.clone(),
            http_client.clone(),
            top_config.app.max_block_age,
            top_config.app.max_block_lag,
            top_config.app.min_synced_rpcs,
            top_config.app.min_sum_soft_limit,
            pending_transactions.clone(),
            Some(pending_tx_sender.clone()),
            vredis_pool.clone(),
            balanced_rpcs,
            Some(watch_consensus_head_sender),
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
            let (private_rpcs, private_handle, _) = Web3Rpcs::spawn(
                block_map,
                top_config.app.chain_id,
                db_conn.clone(),
                http_client.clone(),
                // private rpcs don't get subscriptions, so no need for max_block_age or max_block_lag
                None,
                None,
                0,
                0,
                pending_transactions.clone(),
                // TODO: subscribe to pending transactions on the private rpcs? they seem to have low rate limits, but they should have
                None,
                vredis_pool.clone(),
                private_rpcs,
                // subscribing to new heads here won't work well. if they are fast, they might be ahead of balanced_rpcs
                // they also often have low rate limits
                // however, they are well connected to miners/validators. so maybe using them as a safety check would be good
                // TODO: but maybe we could include privates in the "backup" tier
                None,
            )
            .await
            .context("spawning private_rpcs")?;

            if private_rpcs.by_name.is_empty() {
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
                frontend_ip_rate_limiter = Some(DeferredRateLimiter::<IpAddr>::new(
                    10_000,
                    "ip",
                    rpc_rrl.clone(),
                    None,
                ));
                frontend_registered_user_rate_limiter = Some(DeferredRateLimiter::<u64>::new(
                    10_000, "key", rpc_rrl, None,
                ));
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

        // responses can be very different in sizes, so this is a cache with a max capacity and a weigher
        // TODO: don't allow any response to be bigger than X% of the cache
        let response_cache = Cache::builder()
            .max_capacity(top_config.app.response_cache_max_bytes)
            .weigher(|k: &ResponseCacheKey, v| {
                // TODO: is this good enough?
                if let Ok(v) = serde_json::to_string(v) {
                    let weight = k.weight() + v.len();

                    // the or in unwrap_or is probably never called
                    weight.try_into().unwrap_or(u32::MAX)
                } else {
                    // this seems impossible
                    u32::MAX
                }
            })
            // TODO: what should we set? 10 minutes is arbitrary. the nodes themselves hold onto transactions for much longer
            .time_to_idle(Duration::from_secs(600))
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
            influxdb_client,
            vredis_pool,
            rpc_secret_key_cache,
            bearer_token_semaphores,
            ip_semaphores,
            registered_user_semaphores,
            stat_sender,
        };

        let app = Arc::new(app);

        if important_background_handles.is_empty() {
            info!("no important background handles");

            let f = tokio::spawn(async move {
                let _ = background_shutdown_receiver.recv().await;

                Ok(())
            });

            important_background_handles.push(f);
        }

        Ok((
            app,
            cancellable_handles,
            important_background_handles,
            consensus_connections_watcher,
        )
            .into())
    }

    pub fn head_block_receiver(&self) -> watch::Receiver<Option<Web3ProxyBlock>> {
        self.watch_consensus_head_receiver.clone()
    }

    pub async fn prometheus_metrics(&self) -> String {
        let globals = HashMap::new();
        // TODO: what globals? should this be the hostname or what?
        // globals.insert("service", "web3_proxy");

        // TODO: this needs a refactor to get HELP and TYPE into the serialized text
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

    /// send the request or batch of requests to the approriate RPCs
    pub async fn proxy_web3_rpc(
        self: &Arc<Self>,
        authorization: Arc<Authorization>,
        request: JsonRpcRequestEnum,
        proxy_mode: ProxyMode,
    ) -> Result<(JsonRpcForwardedResponseEnum, Vec<Arc<Web3Rpc>>), FrontendErrorResponse> {
        // trace!(?request, "proxy_web3_rpc");

        // even though we have timeouts on the requests to our backend providers,
        // we need a timeout for the incoming request so that retries don't run forever
        // TODO: take this as an optional argument. per user max? expiration time instead of duration?
        let max_time = Duration::from_secs(120);

        let response = match request {
            JsonRpcRequestEnum::Single(request) => {
                let (response, rpcs) = timeout(
                    max_time,
                    self.proxy_cached_request(&authorization, request, proxy_mode, None),
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
    async fn proxy_web3_rpc_requests(
        self: &Arc<Self>,
        authorization: &Arc<Authorization>,
        requests: Vec<JsonRpcRequest>,
        proxy_mode: ProxyMode,
    ) -> Result<(Vec<JsonRpcForwardedResponse>, Vec<Arc<Web3Rpc>>), FrontendErrorResponse> {
        // TODO: we should probably change ethers-rs to support this directly. they pushed this off to v2 though
        let num_requests = requests.len();

        // TODO: spawn so the requests go in parallel? need to think about rate limiting more if we do that
        // TODO: improve flattening

        // get the head block now so that any requests that need it all use the same block
        // TODO: FrontendErrorResponse that handles "no servers synced" in a consistent way
        // TODO: this still has an edge condition if there is a reorg in the middle of the request!!!
        let head_block_num = self
            .balanced_rpcs
            .head_block_num()
            .context(anyhow::anyhow!("no servers synced"))?;

        let responses = join_all(
            requests
                .into_iter()
                .map(|request| {
                    self.proxy_cached_request(
                        authorization,
                        request,
                        proxy_mode,
                        Some(head_block_num),
                    )
                })
                .collect::<Vec<_>>(),
        )
        .await;

        // TODO: i'm sure this could be done better with iterators
        // TODO: stream the response?
        let mut collected: Vec<JsonRpcForwardedResponse> = Vec::with_capacity(num_requests);
        let mut collected_rpcs: HashSet<Arc<Web3Rpc>> = HashSet::new();
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
    pub fn db_conn(&self) -> Option<DatabaseConnection> {
        self.db_conn.clone()
    }

    pub fn db_replica(&self) -> Option<DatabaseReplica> {
        self.db_replica.clone()
    }

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

    async fn proxy_cached_request(
        self: &Arc<Self>,
        authorization: &Arc<Authorization>,
        mut request: JsonRpcRequest,
        proxy_mode: ProxyMode,
        head_block_num: Option<U64>,
    ) -> Result<(JsonRpcForwardedResponse, Vec<Arc<Web3Rpc>>), FrontendErrorResponse> {
        // trace!("Received request: {:?}", request);

        let request_metadata = Arc::new(RequestMetadata::new(request.num_bytes())?);

        // save the id so we can attach it to the response
        // TODO: instead of cloning, take the id out?
        let request_id = request.id.clone();
        let request_method = request.method.clone();

        // TODO: if eth_chainId or net_version, serve those without querying the backend
        // TODO: don't clone?
        let partial_response: serde_json::Value = match request_method.as_ref() {
            // lots of commands are blocked
            method @ ("db_getHex"
            | "db_getString"
            | "db_putHex"
            | "db_putString"
            | "debug_accountRange"
            | "debug_backtraceAt"
            | "debug_blockProfile"
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
                // i don't think we will ever support these methods
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
            | "eth_pollSubscriptions"
            | "eth_uninstallFilter") => {
                // TODO: unsupported command stat. use the count to prioritize new features
                // TODO: what error code?
                return Ok((
                    // TODO: what code?
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
                match head_block_num.or(self.balanced_rpcs.head_block_num()) {
                    Some(head_block_num) => {
                        json!(head_block_num)
                    }
                    None => {
                        // TODO: what does geth do if this happens?
                        // TODO: i think we want a 502 so that haproxy retries on another server
                        return Err(
                            anyhow::anyhow!("no servers synced. unknown eth_blockNumber").into(),
                        );
                    }
                }
            }
            "eth_chainId" => json!(U64::from(self.config.chain_id)),
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
                serde_json::Value::Bool(false)
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
                    // TODO: maybe having the third party rpcs in their own Web3Rpcs would be good for this
                    ProxyMode::Fastest(x) => Some(x * 4),
                    ProxyMode::Versus => None,
                };

                let (private_rpcs, num) = if let Some(private_rpcs) = self.private_rpcs.as_ref() {
                    if authorization.checks.private_txs {
                        // if we are sending the transaction privately, no matter the proxy_mode, we send to ALL private rpcs
                        (private_rpcs, None)
                    } else {
                        // TODO: send to balanced_rpcs AND private_rpcs
                        (&self.balanced_rpcs, default_num)
                    }
                } else {
                    (&self.balanced_rpcs, default_num)
                };

                let head_block_num = head_block_num
                    .or(self.balanced_rpcs.head_block_num())
                    .ok_or_else(|| anyhow::anyhow!("no servers synced"))?;

                // TODO: error/wait if no head block!

                // try_send_all_upstream_servers puts the request id into the response. no need to do that ourselves here.
                let mut response = private_rpcs
                    .try_send_all_synced_connections(
                        authorization,
                        &request,
                        Some(request_metadata.clone()),
                        Some(&head_block_num),
                        None,
                        Level::Trace,
                        num,
                        true,
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
                serde_json::Value::Bool(false)
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
                serde_json::Value::Bool(true)
            }
            "net_peerCount" => {
                // no stats on this. its cheap
                // TODO: do something with proxy_mode here?
                json!(U64::from(self.balanced_rpcs.num_synced_rpcs()))
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

                        let param = Bytes::from_str(
                            params[0]
                                .as_str()
                                .context("parsing params 0 into str then bytes")?,
                        )
                        .map_err(|x| {
                            trace!("bad request: {:?}", x);
                            FrontendErrorResponse::BadRequest(
                                "param 0 could not be read as H256".to_string(),
                            )
                        })?;

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
                if method.starts_with("admin_") {
                    // TODO: emit a stat? will probably just be noise
                    return Err(FrontendErrorResponse::AccessDenied);
                }

                // emit stats

                // TODO: if no servers synced, wait for them to be synced? probably better to error and let haproxy retry another server
                let head_block_num = head_block_num
                    .or(self.balanced_rpcs.head_block_num())
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
                        from_block: None,
                        to_block: None,
                        method: method.to_string(),
                        params: request.params.clone(),
                        cache_errors: false,
                    }),
                    BlockNeeded::CacheNever => None,
                    BlockNeeded::Cache {
                        block_num,
                        cache_errors,
                    } => {
                        let (request_block_hash, block_depth) = self
                            .balanced_rpcs
                            .block_hash(authorization, &block_num)
                            .await?;

                        if block_depth < self.config.archive_depth {
                            request_metadata
                                .archive_request
                                .store(true, atomic::Ordering::Relaxed);
                        }

                        let request_block = self
                            .balanced_rpcs
                            .block(authorization, &request_block_hash, None)
                            .await?;

                        Some(ResponseCacheKey {
                            from_block: Some(request_block),
                            to_block: None,
                            method: method.to_string(),
                            // TODO: hash here?
                            params: request.params.clone(),
                            cache_errors,
                        })
                    }
                    BlockNeeded::CacheRange {
                        from_block_num,
                        to_block_num,
                        cache_errors,
                    } => {
                        let (from_block_hash, block_depth) = self
                            .balanced_rpcs
                            .block_hash(authorization, &from_block_num)
                            .await?;

                        if block_depth < self.config.archive_depth {
                            request_metadata
                                .archive_request
                                .store(true, atomic::Ordering::Relaxed);
                        }

                        let from_block = self
                            .balanced_rpcs
                            .block(authorization, &from_block_hash, None)
                            .await?;

                        let (to_block_hash, _) = self
                            .balanced_rpcs
                            .block_hash(authorization, &to_block_num)
                            .await?;

                        let to_block = self
                            .balanced_rpcs
                            .block(authorization, &to_block_hash, None)
                            .await?;

                        Some(ResponseCacheKey {
                            from_block: Some(from_block),
                            to_block: Some(to_block),
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
                        let from_block_num = cache_key.from_block.as_ref().map(|x| *x.number());
                        let to_block_num = cache_key.to_block.as_ref().map(|x| *x.number());

                        self.response_cache
                            .try_get_with(cache_key, async move {
                                // TODO: put the hash here instead of the block number? its in the request already.
                                let mut response = self
                                    .balanced_rpcs
                                    .try_proxy_connection(
                                        proxy_mode,
                                        &authorization,
                                        request,
                                        Some(&request_metadata),
                                        from_block_num.as_ref(),
                                        to_block_num.as_ref(),
                                    )
                                    .await?;

                                // discard their id by replacing it with an empty
                                response.id = Default::default();

                                // TODO: only cache the inner response
                                // TODO: how are we going to stream this?
                                // TODO: check response size. if its very large, return it in a custom Error type that bypasses caching? or will moka do that for us?
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
                    let response_stat = RpcQueryStats::new(
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
            let response_stat = RpcQueryStats::new(
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
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("Web3ProxyApp").finish_non_exhaustive()
    }
}
