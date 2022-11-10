// TODO: this file is way too big now. move things into other modules

use crate::app_stats::{ProxyResponseStat, StatEmitter, Web3ProxyStat};
use crate::block_number::block_needed;
use crate::config::{AppConfig, TopConfig};
use crate::frontend::authorization::{AuthorizedRequest, RequestMetadata};
use crate::jsonrpc::JsonRpcForwardedResponse;
use crate::jsonrpc::JsonRpcForwardedResponseEnum;
use crate::jsonrpc::JsonRpcRequest;
use crate::jsonrpc::JsonRpcRequestEnum;
use crate::rpcs::blockchain::{ArcBlock, BlockId};
use crate::rpcs::connections::Web3Connections;
use crate::rpcs::request::OpenRequestHandleMetrics;
use crate::rpcs::transactions::TxStatus;
use anyhow::Context;
use axum::extract::ws::Message;
use axum::headers::{Origin, Referer, UserAgent};
use deferred_rate_limiter::DeferredRateLimiter;
use derive_more::From;
use ethers::core::utils::keccak256;
use ethers::prelude::{Address, Block, Bytes, TxHash, H256, U64};
use futures::future::Abortable;
use futures::future::{join_all, AbortHandle};
use futures::stream::FuturesUnordered;
use futures::stream::StreamExt;
use hashbrown::HashMap;
use ipnet::IpNet;
use metered::{metered, ErrorCount, HitCount, ResponseTime, Throughput};
use migration::{Migrator, MigratorTrait};
use moka::future::Cache;
use redis_rate_limiter::{DeadpoolRuntime, RedisConfig, RedisPool, RedisRateLimiter};
use sea_orm::DatabaseConnection;
use serde::Serialize;
use serde_json::json;
use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::atomic::{self, AtomicUsize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, watch, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_stream::wrappers::{BroadcastStream, WatchStream};
use tracing::{error, info, instrument, trace, warn};
use ulid::Ulid;

// TODO: make this customizable?
static APP_USER_AGENT: &str = concat!(
    "satoshiandkin/",
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
);

/// block hash, method, params
// TODO: better name
type ResponseCacheKey = (H256, String, Option<String>);
type ResponseCache =
    Cache<ResponseCacheKey, JsonRpcForwardedResponse, hashbrown::hash_map::DefaultHashBuilder>;

pub type AnyhowJoinHandle<T> = JoinHandle<anyhow::Result<T>>;

#[derive(Clone, Debug, Default, From)]
pub struct UserKeyData {
    /// database id of the primary user
    pub user_id: u64,
    /// database id of the rpc key
    pub rpc_key_id: u64,
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
    pub frontend_key_rate_limiter: Option<DeferredRateLimiter<Ulid>>,
    pub login_rate_limiter: Option<RedisRateLimiter>,
    pub vredis_pool: Option<RedisPool>,
    // TODO: this key should be our RpcSecretKey class, not Ulid
    pub rpc_secret_key_cache: Cache<Ulid, UserKeyData, hashbrown::hash_map::DefaultHashBuilder>,
    pub rpc_key_semaphores: Cache<u64, Arc<Semaphore>, hashbrown::hash_map::DefaultHashBuilder>,
    pub ip_semaphores: Cache<IpAddr, Arc<Semaphore>, hashbrown::hash_map::DefaultHashBuilder>,
    pub bearer_token_semaphores:
        Cache<String, Arc<Semaphore>, hashbrown::hash_map::DefaultHashBuilder>,
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

/// Connect to the database and run migrations
#[instrument(level = "trace")]
pub async fn get_migrated_db(
    db_url: String,
    min_connections: u32,
    max_connections: u32,
) -> anyhow::Result<DatabaseConnection> {
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

    let db_conn = sea_orm::Database::connect(db_opt).await?;

    // TODO: if error, roll back?
    Migrator::up(&db_conn, None).await?;

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
        if let Some(redirect) = &top_config.app.redirect_user_url {
            assert!(
                redirect.contains("{rpc_key_id}"),
                "redirect_user_url user url must contain \"{{rpc_key_id}}\""
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
            info!("no database");
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
                        ?err,
                        "failed to connect to vredis. some features will be disabled"
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
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::new());

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
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::new());

        // connect to the load balanced rpcs
        let (balanced_rpcs, balanced_handle) = Web3Connections::spawn(
            top_config.app.chain_id,
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
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::new());

        // all the users are the same size, so no need for a weigher
        // if there is no database of users, there will be no keys and so this will be empty
        // TODO: max_capacity from config
        // TODO: ttl from config
        let rpc_secret_key_cache = Cache::builder()
            .max_capacity(10_000)
            .time_to_live(Duration::from_secs(600))
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::new());

        // create semaphores for concurrent connection limits
        // TODO: what should tti be for semaphores?
        let bearer_token_semaphores = Cache::builder()
            .time_to_idle(Duration::from_secs(120))
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::new());
        let ip_semaphores = Cache::builder()
            .time_to_idle(Duration::from_secs(120))
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::new());
        let rpc_key_semaphores = Cache::builder()
            .time_to_idle(Duration::from_secs(120))
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::new());

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

    #[instrument(level = "trace")]
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

    #[measure([ErrorCount, HitCount, ResponseTime, Throughput])]
    #[instrument(level = "trace")]
    pub async fn eth_subscribe<'a>(
        self: &'a Arc<Self>,
        authorized_request: Arc<AuthorizedRequest>,
        payload: JsonRpcRequest,
        subscription_count: &'a AtomicUsize,
        // TODO: taking a sender for Message instead of the exact json we are planning to send feels wrong, but its easier for now
        response_sender: flume::Sender<Message>,
    ) -> anyhow::Result<(AbortHandle, JsonRpcForwardedResponse)> {
        let (subscription_abort_handle, subscription_registration) = AbortHandle::new_pair();

        // TODO: this only needs to be unique per connection. we don't need it globably unique
        let subscription_id = subscription_count.fetch_add(1, atomic::Ordering::SeqCst);
        let subscription_id = U64::from(subscription_id);

        // save the id so we can use it in the response
        let id = payload.id.clone();

        // TODO: calling json! on every request is probably not fast. but we can only match against
        // TODO: i think we need a stricter EthSubscribeRequest type that JsonRpcRequest can turn into
        match payload.params {
            Some(x) if x == json!(["newHeads"]) => {
                let head_block_receiver = self.head_block_receiver.clone();

                trace!(?subscription_id, "new heads subscription");
                tokio::spawn(async move {
                    let mut head_block_receiver = Abortable::new(
                        WatchStream::new(head_block_receiver),
                        subscription_registration,
                    );

                    while let Some(new_head) = head_block_receiver.next().await {
                        // TODO: make a struct for this? using our JsonRpcForwardedResponse won't work because it needs an id
                        let msg = json!({
                            "jsonrpc": "2.0",
                            "method":"eth_subscription",
                            "params": {
                                "subscription": subscription_id,
                                // TODO: option to include full transaction objects instead of just the hashes?
                                "result": new_head.as_ref(),
                            },
                        });

                        // TODO: do clients support binary messages?
                        let msg = Message::Text(
                            serde_json::to_string(&msg).expect("this should always be valid json"),
                        );

                        if response_sender.send_async(msg).await.is_err() {
                            // TODO: cancel this subscription earlier? select on head_block_receiver.next() and an abort handle?
                            break;
                        };
                    }

                    trace!(?subscription_id, "closed new heads subscription");
                });
            }
            Some(x) if x == json!(["newPendingTransactions"]) => {
                let pending_tx_receiver = self.pending_tx_sender.subscribe();

                let mut pending_tx_receiver = Abortable::new(
                    BroadcastStream::new(pending_tx_receiver),
                    subscription_registration,
                );

                trace!(?subscription_id, "pending transactions subscription");
                tokio::spawn(async move {
                    while let Some(Ok(new_tx_state)) = pending_tx_receiver.next().await {
                        let new_tx = match new_tx_state {
                            TxStatus::Pending(tx) => tx,
                            TxStatus::Confirmed(..) => continue,
                            TxStatus::Orphaned(tx) => tx,
                        };

                        // TODO: make a struct for this? using our JsonRpcForwardedResponse won't work because it needs an id
                        let msg = json!({
                            "jsonrpc": "2.0",
                            "method": "eth_subscription",
                            "params": {
                                "subscription": subscription_id,
                                "result": new_tx.hash,
                            },
                        });

                        let msg =
                            Message::Text(serde_json::to_string(&msg).expect("we made this `msg`"));

                        if response_sender.send_async(msg).await.is_err() {
                            // TODO: cancel this subscription earlier? select on head_block_receiver.next() and an abort handle?
                            break;
                        };
                    }

                    trace!(?subscription_id, "closed new heads subscription");
                });
            }
            Some(x) if x == json!(["newPendingFullTransactions"]) => {
                // TODO: too much copy/pasta with newPendingTransactions
                let pending_tx_receiver = self.pending_tx_sender.subscribe();

                let mut pending_tx_receiver = Abortable::new(
                    BroadcastStream::new(pending_tx_receiver),
                    subscription_registration,
                );

                trace!(?subscription_id, "pending transactions subscription");

                // TODO: do something with this handle?
                tokio::spawn(async move {
                    while let Some(Ok(new_tx_state)) = pending_tx_receiver.next().await {
                        let new_tx = match new_tx_state {
                            TxStatus::Pending(tx) => tx,
                            TxStatus::Confirmed(..) => continue,
                            TxStatus::Orphaned(tx) => tx,
                        };

                        // TODO: make a struct for this? using our JsonRpcForwardedResponse won't work because it needs an id
                        let msg = json!({
                            "jsonrpc": "2.0",
                            "method": "eth_subscription",
                            "params": {
                                "subscription": subscription_id,
                                // upstream just sends the txid, but we want to send the whole transaction
                                "result": new_tx,
                            },
                        });

                        let msg = Message::Text(
                            serde_json::to_string(&msg).expect("we made this message"),
                        );

                        if response_sender.send_async(msg).await.is_err() {
                            // TODO: cancel this subscription earlier? select on head_block_receiver.next() and an abort handle?
                            break;
                        };
                    }

                    trace!(?subscription_id, "closed new heads subscription");
                });
            }
            Some(x) if x == json!(["newPendingRawTransactions"]) => {
                // TODO: too much copy/pasta with newPendingTransactions
                let pending_tx_receiver = self.pending_tx_sender.subscribe();

                let mut pending_tx_receiver = Abortable::new(
                    BroadcastStream::new(pending_tx_receiver),
                    subscription_registration,
                );

                trace!(?subscription_id, "pending transactions subscription");

                // TODO: do something with this handle?
                tokio::spawn(async move {
                    while let Some(Ok(new_tx_state)) = pending_tx_receiver.next().await {
                        let new_tx = match new_tx_state {
                            TxStatus::Pending(tx) => tx,
                            TxStatus::Confirmed(..) => continue,
                            TxStatus::Orphaned(tx) => tx,
                        };

                        // TODO: make a struct for this? using our JsonRpcForwardedResponse won't work because it needs an id
                        let msg = json!({
                            "jsonrpc": "2.0",
                            "method": "eth_subscription",
                            "params": {
                                "subscription": subscription_id,
                                // upstream just sends the txid, but we want to send the raw transaction
                                "result": new_tx.rlp(),
                            },
                        });

                        let msg = Message::Text(
                            serde_json::to_string(&msg).expect("this message was just built"),
                        );

                        if response_sender.send_async(msg).await.is_err() {
                            // TODO: cancel this subscription earlier? select on head_block_receiver.next() and an abort handle?
                            break;
                        };
                    }

                    trace!(?subscription_id, "closed new heads subscription");
                });
            }
            _ => return Err(anyhow::anyhow!("unimplemented")),
        }

        // TODO: do something with subscription_join_handle?

        let response = JsonRpcForwardedResponse::from_value(json!(subscription_id), id);

        // TODO: make a `SubscriptonHandle(AbortHandle, JoinHandle)` struct?

        Ok((subscription_abort_handle, response))
    }

    /// send the request or batch of requests to the approriate RPCs
    #[instrument(level = "trace")]
    pub async fn proxy_web3_rpc(
        self: &Arc<Self>,
        authorized_request: Arc<AuthorizedRequest>,
        request: JsonRpcRequestEnum,
    ) -> anyhow::Result<JsonRpcForwardedResponseEnum> {
        // TODO: this should probably be trace level
        trace!(?request, "proxy_web3_rpc");

        // even though we have timeouts on the requests to our backend providers,
        // we need a timeout for the incoming request so that retries don't run forever
        // TODO: take this as an optional argument. per user max? expiration time instead of duration?
        let max_time = Duration::from_secs(120);

        let response = match request {
            JsonRpcRequestEnum::Single(request) => JsonRpcForwardedResponseEnum::Single(
                timeout(
                    max_time,
                    self.proxy_web3_rpc_request(authorized_request, request),
                )
                .await??,
            ),
            JsonRpcRequestEnum::Batch(requests) => JsonRpcForwardedResponseEnum::Batch(
                timeout(
                    max_time,
                    self.proxy_web3_rpc_requests(authorized_request, requests),
                )
                .await??,
            ),
        };

        // TODO: this should probably be trace level
        trace!(?response, "Forwarding");

        Ok(response)
    }

    /// cut up the request and send to potentually different servers
    /// TODO: make sure this isn't a problem
    #[instrument(level = "trace")]
    async fn proxy_web3_rpc_requests(
        self: &Arc<Self>,
        authorized_request: Arc<AuthorizedRequest>,
        requests: Vec<JsonRpcRequest>,
    ) -> anyhow::Result<Vec<JsonRpcForwardedResponse>> {
        // TODO: we should probably change ethers-rs to support this directly
        let num_requests = requests.len();

        let responses = join_all(
            requests
                .into_iter()
                .map(|request| {
                    let authorized_request = authorized_request.clone();

                    // TODO: spawn so the requests go in parallel
                    // TODO: i think we will need to flatten
                    self.proxy_web3_rpc_request(authorized_request, request)
                })
                .collect::<Vec<_>>(),
        )
        .await;

        // TODO: i'm sure this could be done better with iterators
        let mut collected: Vec<JsonRpcForwardedResponse> = Vec::with_capacity(num_requests);
        for response in responses {
            collected.push(response?);
        }

        Ok(collected)
    }

    /// TODO: i don't think we want or need this. just use app.db_conn, or maybe app.db_conn.clone() or app.db_conn.as_ref()
    #[instrument(level = "trace")]
    pub fn db_conn(&self) -> Option<DatabaseConnection> {
        self.db_conn.clone()
    }

    #[instrument(level = "trace")]
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
    #[instrument(level = "trace")]
    async fn proxy_web3_rpc_request(
        self: &Arc<Self>,
        authorized_request: Arc<AuthorizedRequest>,
        mut request: JsonRpcRequest,
    ) -> anyhow::Result<JsonRpcForwardedResponse> {
        trace!("Received request: {:?}", request);

        // TODO: allow customizing the period?
        let request_metadata = Arc::new(RequestMetadata::new(60, &request)?);

        // save the id so we can attach it to the response
        // TODO: instead of cloning, take the id out
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
                        Some(&authorized_request),
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
                    method,
                    request.params.as_mut(),
                    head_block_id.num,
                    &self.balanced_rpcs,
                )
                .await?
                {
                    // TODO: maybe this should be on the app and not on balanced_rpcs
                    let (request_block_hash, archive_needed) =
                        self.balanced_rpcs.block_hash(&request_block_needed).await?;

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

                    let authorized_request = authorized_request.clone();

                    self.response_cache
                        .try_get_with(cache_key, async move {
                            // TODO: retry some failures automatically!
                            // TODO: try private_rpcs if all the balanced_rpcs fail!
                            // TODO: put the hash here instead?
                            let mut response = self
                                .balanced_rpcs
                                .try_send_best_upstream_server(
                                    Some(&authorized_request),
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

                // DRY this up by just returning the partial result (or error) here
                if let (Some(stat_sender), Ok(AuthorizedRequest::User(Some(_), authorized_key))) = (
                    self.stat_sender.as_ref(),
                    Arc::try_unwrap(authorized_request),
                ) {
                    let response_stat = ProxyResponseStat::new(
                        method.to_string(),
                        authorized_key,
                        request_metadata,
                        &response,
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

        if let (Some(stat_sender), Ok(AuthorizedRequest::User(Some(_), authorized_key))) = (
            self.stat_sender.as_ref(),
            Arc::try_unwrap(authorized_request),
        ) {
            let response_stat =
                ProxyResponseStat::new(request.method, authorized_key, request_metadata, &response);

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
