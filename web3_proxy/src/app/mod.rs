// TODO: this file is way too big now. move things into other modules
mod ws;

use crate::app_stats::{ProxyResponseStat, StatEmitter, Web3ProxyStat};
use crate::block_number::block_needed;
use crate::config::{AppConfig, TopConfig};
use crate::frontend::authorization::{Authorization, RequestMetadata};
use crate::jsonrpc::JsonRpcForwardedResponse;
use crate::jsonrpc::JsonRpcForwardedResponseEnum;
use crate::jsonrpc::JsonRpcRequest;
use crate::jsonrpc::JsonRpcRequestEnum;
use crate::rpcs::blockchain::{ArcBlock, BlockId};
use crate::rpcs::connections::Web3Connections;
use crate::rpcs::request::OpenRequestHandleMetrics;
use crate::rpcs::transactions::TxStatus;
use anyhow::Context;
use axum::headers::{Origin, Referer, UserAgent};
use deferred_rate_limiter::DeferredRateLimiter;
use derive_more::From;
use ethers::core::utils::keccak256;
use ethers::prelude::{Address, Block, Bytes, TxHash, H256, U64};
use futures::future::join_all;
use futures::stream::FuturesUnordered;
use futures::stream::StreamExt;
use hashbrown::HashMap;
use ipnet::IpNet;
use log::{debug, error, info, warn};
use metered::{metered, ErrorCount, HitCount, ResponseTime, Throughput};
use migration::sea_orm::{self, ConnectionTrait, Database, DatabaseConnection};
use migration::sea_query::table::ColumnDef;
use migration::{Alias, DbErr, Migrator, MigratorTrait, Table};
use moka::future::Cache;
use redis_rate_limiter::{DeadpoolRuntime, RedisConfig, RedisPool, RedisRateLimiter};
use serde::Serialize;
use serde_json::json;
use std::fmt;
use std::net::IpAddr;
use std::num::NonZeroU64;
use std::str::FromStr;
use std::sync::atomic;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, watch, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use ulid::Ulid;

// TODO: make this customizable?
pub static APP_USER_AGENT: &str = concat!(
    "satoshiandkin/",
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
);

/// TODO: allow customizing the request period?
pub static REQUEST_PERIOD: u64 = 60;

/// block hash, method, params
// TODO: better name
type ResponseCacheKey = (H256, String, Option<String>);
type ResponseCache =
    Cache<ResponseCacheKey, JsonRpcForwardedResponse, hashbrown::hash_map::DefaultHashBuilder>;

pub type AnyhowJoinHandle<T> = JoinHandle<anyhow::Result<T>>;

#[derive(Clone, Debug, Default, From)]
pub struct AuthorizationChecks {
    /// database id of the primary user.
    /// TODO: do we need this? its on the authorization so probably not
    pub user_id: u64,
    /// database id of the rpc key
    /// if this is None, then this request is being rate limited by ip
    pub rpc_key_id: Option<NonZeroU64>,
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
    /// Chance to save reverting eth_call, eth_estimateGas, and eth_sendRawTransaction to the database.
    /// TODO: f32 would be fine
    pub log_revert_chance: f64,
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
    head_block_receiver: watch::Receiver<ArcBlock>,
    pending_tx_sender: broadcast::Sender<TxStatus>,
    pub config: AppConfig,
    pub db_conn: Option<sea_orm::DatabaseConnection>,
    /// prometheus metrics
    app_metrics: Arc<Web3ProxyAppMetrics>,
    open_request_handle_metrics: Arc<OpenRequestHandleMetrics>,
    /// store pending transactions that we've seen so that we don't send duplicates to subscribers
    pub pending_transactions: Cache<TxHash, TxStatus, hashbrown::hash_map::DefaultHashBuilder>,
    pub frontend_ip_rate_limiter: Option<DeferredRateLimiter<IpAddr>>,
    // TODO: this key should be our RpcSecretKey class, not Ulid
    pub frontend_key_rate_limiter: Option<DeferredRateLimiter<Ulid>>,
    pub login_rate_limiter: Option<RedisRateLimiter>,
    pub vredis_pool: Option<RedisPool>,
    // TODO: this key should be our RpcSecretKey class, not Ulid
    pub rpc_secret_key_cache:
        Cache<Ulid, AuthorizationChecks, hashbrown::hash_map::DefaultHashBuilder>,
    pub rpc_key_semaphores:
        Cache<NonZeroU64, Arc<Semaphore>, hashbrown::hash_map::DefaultHashBuilder>,
    pub ip_semaphores: Cache<IpAddr, Arc<Semaphore>, hashbrown::hash_map::DefaultHashBuilder>,
    pub bearer_token_semaphores:
        Cache<String, Arc<Semaphore>, hashbrown::hash_map::DefaultHashBuilder>,
    pub stat_sender: Option<flume::Sender<Web3ProxyStat>>,
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

    Ok(())
}

/// Connect to the database and run migrations
pub async fn get_migrated_db(
    db_url: String,
    min_connections: u32,
    max_connections: u32,
) -> anyhow::Result<DatabaseConnection> {
    let db_conn = get_db(db_url, min_connections, max_connections).await?;

    let db_backend = db_conn.get_database_backend();

    // TODO: put the timestamp into this?
    let create_lock_statment = db_backend.build(
        Table::create()
            .table(Alias::new("migration_lock"))
            .col(ColumnDef::new(Alias::new("locked")).boolean().default(true)),
    );

    loop {
        if Migrator::get_pending_migrations(&db_conn).await?.is_empty() {
            info!("no migrations to apply");
            return Ok(db_conn);
        }

        // there are migrations to apply
        // acquire a lock
        if let Err(err) = db_conn.execute(create_lock_statment.clone()).await {
            debug!("Unable to acquire lock. err={:?}", err);

            // TODO: exponential backoff with jitter
            sleep(Duration::from_secs(1)).await;

            continue;
        }

        debug!("migration lock acquired");
        break;
    }

    let migration_result = Migrator::up(&db_conn, None).await;

    // drop the distributed lock
    drop_migration_lock(&db_conn).await?;

    // return if migrations erred
    migration_result?;

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

        // setup metrics
        let app_metrics = Default::default();
        let open_request_handle_metrics: Arc<OpenRequestHandleMetrics> = Default::default();

        // connect to mysql and make sure the latest migrations have run
        let db_conn = if let Some(db_url) = top_config.app.db_url.clone() {
            let db_min_connections = top_config
                .app
                .db_min_connections
                .unwrap_or(num_workers as u32);

            // TODO: what default multiple?
            let db_max_connections = top_config
                .app
                .db_max_connections
                .unwrap_or(db_min_connections * 2);

            let db_conn = get_migrated_db(db_url, db_min_connections, db_max_connections).await?;

            Some(db_conn)
        } else {
            warn!("no database. some features will be disabled");
            None
        };

        let balanced_rpcs = top_config.balanced_rpcs;
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
        let (head_block_sender, head_block_receiver) = watch::channel(Arc::new(Block::default()));
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
            Some(head_block_sender),
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
        let mut frontend_key_rate_limiter = None;
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
            frontend_key_rate_limiter = Some(DeferredRateLimiter::<Ulid>::new(
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
            .weigher(|k: &(H256, String, Option<String>), v| {
                // TODO: make this weigher past. serializing json is not fast
                let mut size = (k.1).len();

                if let Some(params) = &k.2 {
                    size += params.len()
                }

                if let Ok(v) = serde_json::to_string(v) {
                    size += v.len();

                    // the or in unwrap_or is probably never called
                    size.try_into().unwrap_or(u32::MAX)
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
        let rpc_key_semaphores = Cache::builder()
            .time_to_idle(Duration::from_secs(120))
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default());

        let app = Self {
            config: top_config.app,
            balanced_rpcs,
            private_rpcs,
            response_cache,
            head_block_receiver,
            pending_tx_sender,
            pending_transactions,
            frontend_ip_rate_limiter,
            frontend_key_rate_limiter,
            login_rate_limiter,
            db_conn,
            vredis_pool,
            app_metrics,
            open_request_handle_metrics,
            rpc_secret_key_cache,
            bearer_token_semaphores,
            ip_semaphores,
            rpc_key_semaphores,
            stat_sender,
        };

        let app = Arc::new(app);

        Ok((app, cancellable_handles, important_background_handles).into())
    }

    pub fn prometheus_metrics(&self) -> String {
        let globals = HashMap::new();
        // TODO: what globals? should this be the hostname or what?
        // globals.insert("service", "web3_proxy");

        #[derive(Serialize)]
        struct CombinedMetrics<'a> {
            app: &'a Web3ProxyAppMetrics,
            backend_rpc: &'a OpenRequestHandleMetrics,
        }

        let metrics = CombinedMetrics {
            app: &self.app_metrics,
            backend_rpc: &self.open_request_handle_metrics,
        };

        serde_prometheus::to_string(&metrics, Some("web3_proxy"), globals)
            .expect("prometheus metrics should always serialize")
    }

    /// send the request or batch of requests to the approriate RPCs
    pub async fn proxy_web3_rpc(
        self: &Arc<Self>,
        authorization: Arc<Authorization>,
        request: JsonRpcRequestEnum,
    ) -> anyhow::Result<JsonRpcForwardedResponseEnum> {
        // TODO: this should probably be trace level
        // // trace!(?request, "proxy_web3_rpc");

        // even though we have timeouts on the requests to our backend providers,
        // we need a timeout for the incoming request so that retries don't run forever
        // TODO: take this as an optional argument. per user max? expiration time instead of duration?
        let max_time = Duration::from_secs(120);

        let response = match request {
            JsonRpcRequestEnum::Single(request) => JsonRpcForwardedResponseEnum::Single(
                timeout(
                    max_time,
                    self.proxy_web3_rpc_request(&authorization, request),
                )
                .await??,
            ),
            JsonRpcRequestEnum::Batch(requests) => JsonRpcForwardedResponseEnum::Batch(
                timeout(
                    max_time,
                    self.proxy_web3_rpc_requests(&authorization, requests),
                )
                .await??,
            ),
        };

        // TODO: this should probably be trace level
        // // trace!(?response, "Forwarding");

        Ok(response)
    }

    /// cut up the request and send to potentually different servers
    /// TODO: make sure this isn't a problem
    async fn proxy_web3_rpc_requests(
        self: &Arc<Self>,
        authorization: &Arc<Authorization>,
        requests: Vec<JsonRpcRequest>,
    ) -> anyhow::Result<Vec<JsonRpcForwardedResponse>> {
        // TODO: we should probably change ethers-rs to support this directly
        let num_requests = requests.len();

        // TODO: spawn so the requests go in parallel
        // TODO: i think we will need to flatten
        let responses = join_all(
            requests
                .into_iter()
                .map(|request| self.proxy_web3_rpc_request(authorization, request))
                .collect::<Vec<_>>(),
        )
        .await;

        // TODO: i'm sure this could be done better with iterators. we could return the error earlier then, too
        // TODO: stream the response?
        let mut collected: Vec<JsonRpcForwardedResponse> = Vec::with_capacity(num_requests);
        for response in responses {
            collected.push(response?);
        }

        Ok(collected)
    }

    /// TODO: i don't think we want or need this. just use app.db_conn, or maybe app.db_conn.clone() or app.db_conn.as_ref()
    pub fn db_conn(&self) -> Option<DatabaseConnection> {
        self.db_conn.clone()
    }

    pub async fn redis_conn(&self) -> anyhow::Result<redis_rate_limiter::RedisConnection> {
        match self.vredis_pool.as_ref() {
            None => Err(anyhow::anyhow!("no redis server configured")),
            Some(redis_pool) => {
                let redis_conn = redis_pool.get().await?;

                Ok(redis_conn)
            }
        }
    }

    #[measure([ErrorCount, HitCount, ResponseTime, Throughput])]
    async fn proxy_web3_rpc_request(
        self: &Arc<Self>,
        authorization: &Arc<Authorization>,
        mut request: JsonRpcRequest,
    ) -> anyhow::Result<JsonRpcForwardedResponse> {
        // trace!("Received request: {:?}", request);

        let request_metadata = Arc::new(RequestMetadata::new(REQUEST_PERIOD, request.num_bytes())?);

        // save the id so we can attach it to the response
        // TODO: instead of cloning, take the id out?
        let request_id = request.id.clone();

        // TODO: if eth_chainId or net_version, serve those without querying the backend
        // TODO: don't clone?
        let partial_response: serde_json::Value = match request.method.clone().as_ref() {
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
                // TODO: proper error code
                return Err(anyhow::anyhow!("method unsupported: {}", method));
            }
            // TODO: implement these commands
            method @ ("eth_getFilterChanges"
            | "eth_getFilterLogs"
            | "eth_newBlockFilter"
            | "eth_newFilter"
            | "eth_newPendingTransactionFilter"
            | "eth_uninstallFilter") => {
                // TODO: unsupported command stat
                return Err(anyhow::anyhow!("not yet implemented: {}", method));
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
            // TODO: eth_callBundle (https://docs.flashbots.net/flashbots-auction/searchers/advanced/rpc-endpoint#eth_callbundle)
            // TODO: eth_cancelPrivateTransaction (https://docs.flashbots.net/flashbots-auction/searchers/advanced/rpc-endpoint#eth_cancelprivatetransaction, but maybe just reject)
            // TODO: eth_sendPrivateTransaction (https://docs.flashbots.net/flashbots-auction/searchers/advanced/rpc-endpoint#eth_sendprivatetransaction)
            "eth_coinbase" => {
                // no need for serving coinbase
                // we could return a per-user payment address here, but then we might leak that to dapps
                // no stats on this. its cheap
                json!(Address::zero())
            }
            // TODO: eth_estimateGas using anvil?
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
                // emit stats
                let rpcs = self.private_rpcs.as_ref().unwrap_or(&self.balanced_rpcs);

                return rpcs
                    .try_send_all_upstream_servers(
                        authorization,
                        request,
                        Some(request_metadata),
                        None,
                    )
                    .await;
            }
            "eth_syncing" => {
                // no stats on this. its cheap
                // TODO: return a real response if all backends are syncing or if no servers in sync
                json!(false)
            }
            "net_listening" => {
                // no stats on this. its cheap
                // TODO: only if there are some backends on balanced_rpcs?
                json!(true)
            }
            "net_peerCount" => {
                // emit stats
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
                            // TODO: this needs the correct error code in the response
                            return Err(anyhow::anyhow!("invalid request"));
                        }

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
                        return Err(anyhow::anyhow!("invalid request"));
                    }
                }
            }
            // anything else gets sent to backend rpcs and cached
            method => {
                // emit stats

                // TODO: if no servers synced, wait for them to be synced?
                let head_block_id = self
                    .balanced_rpcs
                    .head_block_id()
                    .context("no servers synced")?;

                // we do this check before checking caches because it might modify the request params
                // TODO: add a stat for archive vs full since they should probably cost different
                let request_block_id = if let Some(request_block_needed) = block_needed(
                    authorization,
                    method,
                    request.params.as_mut(),
                    head_block_id.num,
                    &self.balanced_rpcs,
                )
                .await?
                {
                    // TODO: maybe this should be on the app and not on balanced_rpcs
                    let (request_block_hash, archive_needed) = self
                        .balanced_rpcs
                        .block_hash(authorization, &request_block_needed)
                        .await?;

                    if archive_needed {
                        request_metadata
                            .archive_request
                            .store(true, atomic::Ordering::Relaxed);
                    }

                    BlockId {
                        num: request_block_needed,
                        hash: request_block_hash,
                    }
                } else {
                    head_block_id
                };

                // TODO: struct for this?
                // TODO: this can be rather large. is that okay?
                let cache_key = (
                    request_block_id.hash,
                    request.method.clone(),
                    request.params.clone().map(|x| x.to_string()),
                );

                let mut response = {
                    let request_metadata = request_metadata.clone();

                    let authorization = authorization.clone();

                    self.response_cache
                        .try_get_with(cache_key, async move {
                            // TODO: retry some failures automatically!
                            // TODO: try private_rpcs if all the balanced_rpcs fail!
                            // TODO: put the hash here instead?
                            let mut response = self
                                .balanced_rpcs
                                .try_send_best_upstream_server(
                                    &authorization,
                                    request,
                                    Some(&request_metadata),
                                    Some(&request_block_id.num),
                                )
                                .await?;

                            // discard their id by replacing it with an empty
                            response.id = Default::default();

                            // TODO: only cache the inner response (or error)
                            Ok::<_, anyhow::Error>(response)
                        })
                        .await
                        // TODO: what is the best way to handle an Arc here?
                        .map_err(|err| {
                            // TODO: emit a stat for an error
                            anyhow::anyhow!(err)
                        })
                        .context("caching response")?
                };

                // since this data came likely out of a cache, the id is not going to match
                // replace the id with our request's id.
                response.id = request_id;

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

                return Ok(response);
            }
        };

        let response = JsonRpcForwardedResponse::from_value(partial_response, request_id);

        if let Some(stat_sender) = self.stat_sender.as_ref() {
            let response_stat = ProxyResponseStat::new(
                request.method,
                authorization.clone(),
                request_metadata,
                response.num_bytes(),
            );

            stat_sender
                .send_async(response_stat.into())
                .await
                .context("stat_sender sending response stat")?;
        }

        Ok(response)
    }
}

impl fmt::Debug for Web3ProxyApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("Web3ProxyApp").finish_non_exhaustive()
    }
}
