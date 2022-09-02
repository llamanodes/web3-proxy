// TODO: this file is way too big now. move things into other modules

use crate::block_number::block_needed;
use crate::config::{AppConfig, TopConfig};
use crate::jsonrpc::JsonRpcForwardedResponse;
use crate::jsonrpc::JsonRpcForwardedResponseEnum;
use crate::jsonrpc::JsonRpcRequest;
use crate::jsonrpc::JsonRpcRequestEnum;
use crate::rpcs::blockchain::{ArcBlock, BlockHashesMap};
use crate::rpcs::connections::Web3Connections;
use crate::rpcs::transactions::TxStatus;
use crate::stats::AppStats;
use anyhow::Context;
use axum::extract::ws::Message;
use dashmap::mapref::entry::Entry as DashMapEntry;
use dashmap::DashMap;
use derive_more::From;
use ethers::core::utils::keccak256;
use ethers::prelude::{Address, Block, Bytes, TxHash, H256, U64};
use fifomap::{FifoCountMap, FifoSizedMap};
use futures::future::Abortable;
use futures::future::{join_all, AbortHandle};
use futures::stream::FuturesUnordered;
use futures::stream::StreamExt;
use futures::Future;
use migration::{Migrator, MigratorTrait};
use parking_lot::RwLock;
use redis_rate_limit::bb8::PooledConnection;
use redis_rate_limit::{
    bb8::{self, ErrorSink},
    RedisConnectionManager, RedisErrorSink, RedisPool, RedisRateLimit,
};
use sea_orm::DatabaseConnection;
use serde_json::json;
use std::fmt;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::{self, AtomicUsize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Instant};
use tokio_stream::wrappers::{BroadcastStream, WatchStream};
use tracing::{info, info_span, instrument, trace, warn, Instrument};
use uuid::Uuid;

// TODO: make this customizable?
static APP_USER_AGENT: &str = concat!(
    "satoshiandkin/",
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
);

/// block hash, method, params
// TODO: better name
type CacheKey = (H256, String, Option<String>);

type ResponseLrcCache = RwLock<FifoSizedMap<CacheKey, JsonRpcForwardedResponse>>;

type ActiveRequestsMap = DashMap<CacheKey, watch::Receiver<bool>>;

pub type AnyhowJoinHandle<T> = JoinHandle<anyhow::Result<T>>;

#[derive(Clone, Copy, From)]
pub struct UserCacheValue {
    pub expires_at: Instant,
    pub user_id: u64,
    pub user_count_per_period: u64,
}

/// The application
// TODO: this debug impl is way too verbose. make something smaller
// TODO: i'm sure this is more arcs than necessary, but spawning futures makes references hard
pub struct Web3ProxyApp {
    /// Send requests to the best server available
    pub balanced_rpcs: Arc<Web3Connections>,
    /// Send private requests (like eth_sendRawTransaction) to all these servers
    pub private_rpcs: Arc<Web3Connections>,
    /// Track active requests so that we don't send the same query to multiple backends
    pub active_requests: ActiveRequestsMap,
    response_cache: ResponseLrcCache,
    // don't drop this or the sender will stop working
    // TODO: broadcast channel instead?
    head_block_receiver: watch::Receiver<ArcBlock>,
    pending_tx_sender: broadcast::Sender<TxStatus>,
    pub config: AppConfig,
    pub db_conn: Option<sea_orm::DatabaseConnection>,
    pub pending_transactions: Arc<DashMap<TxHash, TxStatus>>,
    pub rate_limiter: Option<RedisRateLimit>,
    pub redis_pool: Option<RedisPool>,
    pub stats: AppStats,
    pub user_cache: RwLock<FifoCountMap<Uuid, UserCacheValue>>,
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

/// Connect to the database and run migrations
pub async fn get_migrated_db(
    db_url: String,
    min_connections: u32,
    max_connections: u32,
) -> anyhow::Result<DatabaseConnection> {
    let mut db_opt = sea_orm::ConnectOptions::new(db_url);

    // TODO: load all these options from the config file. i think mysql default max is 100
    // TODO: sqlx logging only in debug. way too verbose for production
    db_opt
        .min_connections(min_connections)
        .max_connections(max_connections)
        .connect_timeout(Duration::from_secs(8))
        .idle_timeout(Duration::from_secs(8))
        .max_lifetime(Duration::from_secs(60))
        .sqlx_logging(false);
    // .sqlx_logging_level(log::LevelFilter::Info);

    let db = sea_orm::Database::connect(db_opt).await?;

    // TODO: if error, roll back?
    Migrator::up(&db, None).await?;

    Ok(db)
}

impl Web3ProxyApp {
    pub async fn spawn(
        app_stats: AppStats,
        top_config: TopConfig,
    ) -> anyhow::Result<(
        Arc<Web3ProxyApp>,
        Pin<Box<dyn Future<Output = anyhow::Result<()>>>>,
    )> {
        // safety checks on the config
        assert!(
            top_config.app.redirect_user_url.contains("{{user_id}}"),
            "redirect user url must contain \"{{user_id}}\""
        );

        // first, we connect to mysql and make sure the latest migrations have run
        let db_conn = if let Some(db_url) = &top_config.app.db_url {
            let db_min_connections = top_config.app.db_min_connections;

            // TODO: what default multiple?
            let redis_max_connections = top_config
                .app
                .db_max_connections
                .unwrap_or(db_min_connections * 4);

            let db =
                get_migrated_db(db_url.clone(), db_min_connections, redis_max_connections).await?;

            Some(db)
        } else {
            info!("no database");
            None
        };

        let balanced_rpcs = top_config.balanced_rpcs;

        let private_rpcs = if let Some(private_rpcs) = top_config.private_rpcs {
            private_rpcs
        } else {
            Default::default()
        };

        // TODO: try_join_all instead?
        let handles = FuturesUnordered::new();

        // make a http shared client
        // TODO: can we configure the connection pool? should we?
        // TODO: 5 minutes is probably long enough. unlimited is a bad idea if something is wrong with the remote server
        let http_client = Some(
            reqwest::ClientBuilder::new()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(60))
                .user_agent(APP_USER_AGENT)
                .build()?,
        );

        let redis_pool = match top_config.app.redis_url.as_ref() {
            Some(redis_url) => {
                info!("Connecting to redis on {}", redis_url);

                let manager = RedisConnectionManager::new(redis_url.as_ref())?;

                let redis_min_connections = top_config.app.redis_min_connections;

                let redis_max_connections = top_config
                    .app
                    .redis_max_connections
                    .unwrap_or(redis_min_connections * 4);

                // TODO: min_idle?
                // TODO: set max_size based on max expected concurrent connections? set based on num_workers?
                let builder = bb8::Pool::builder()
                    .error_sink(RedisErrorSink.boxed_clone())
                    .min_idle(Some(redis_min_connections))
                    .max_size(redis_max_connections);

                let pool = builder.build(manager).await?;

                Some(pool)
            }
            None => {
                warn!("no redis connection");
                None
            }
        };

        // TODO: this should be a broadcast channel
        let (head_block_sender, head_block_receiver) = watch::channel(Arc::new(Block::default()));
        // TODO: will one receiver lagging be okay? how big should this be?
        let (pending_tx_sender, pending_tx_receiver) = broadcast::channel(256);

        // TODO: use this? it could listen for confirmed transactions and then clear pending_transactions, but the head_block_sender is doing that
        drop(pending_tx_receiver);

        // TODO: this will grow unbounded!! add some expiration to this. and probably move to redis
        let pending_transactions = Arc::new(DashMap::new());

        // TODO: don't drop the pending_tx_receiver. instead, read it to mark transactions as "seen". once seen, we won't re-send them
        // TODO: once a transaction is "Confirmed" we remove it from the map. this should prevent major memory leaks.
        // TODO: we should still have some sort of expiration or maximum size limit for the map

        // this block map is shared between balanced_rpcs and private_rpcs.
        let block_map = BlockHashesMap::default();

        let (balanced_rpcs, balanced_handle) = Web3Connections::spawn(
            top_config.app.chain_id,
            balanced_rpcs,
            http_client.clone(),
            redis_pool.clone(),
            block_map.clone(),
            Some(head_block_sender),
            top_config.app.min_sum_soft_limit,
            top_config.app.min_synced_rpcs,
            Some(pending_tx_sender.clone()),
            pending_transactions.clone(),
        )
        .await
        .context("balanced rpcs")?;

        handles.push(balanced_handle);

        let private_rpcs = if private_rpcs.is_empty() {
            warn!("No private relays configured. Any transactions will be broadcast to the public mempool!");
            balanced_rpcs.clone()
        } else {
            // TODO: attach context to this error
            let (private_rpcs, private_handle) = Web3Connections::spawn(
                top_config.app.chain_id,
                private_rpcs,
                http_client.clone(),
                redis_pool.clone(),
                block_map,
                // subscribing to new heads here won't work well. if they are fast, they might be ahead of balanced_rpcs
                None,
                // minimum doesn't really matter on private rpcs
                1,
                1,
                // TODO: subscribe to pending transactions on the private rpcs? they seem to have low rate limits
                None,
                pending_transactions.clone(),
            )
            .await
            .context("private_rpcs")?;

            handles.push(private_handle);

            private_rpcs
        };

        let frontend_rate_limiter = redis_pool.as_ref().map(|redis_pool| {
            RedisRateLimit::new(
                redis_pool.clone(),
                "web3_proxy",
                "frontend",
                top_config.app.public_rate_limit_per_minute,
                60.0,
            )
        });

        // keep the borrow checker happy
        let response_cache_max_bytes = top_config.app.response_cache_max_bytes;

        let app = Self {
            config: top_config.app,
            balanced_rpcs,
            private_rpcs,
            active_requests: Default::default(),
            // TODO: make the share configurable. or maybe take a number as bytes?
            response_cache: RwLock::new(FifoSizedMap::new(response_cache_max_bytes, 100)),
            head_block_receiver,
            pending_tx_sender,
            pending_transactions,
            rate_limiter: frontend_rate_limiter,
            db_conn,
            redis_pool,
            stats: app_stats,
            // TODO: make the size configurable
            // TODO: why does this need to be async but the other one doesn't?
            user_cache: RwLock::new(FifoCountMap::new(1_000)),
        };

        let app = Arc::new(app);

        // create a handle that returns on the first error
        // TODO: move this to a helper. i think Web3Connections needs it too
        let handle = Box::pin(flatten_handles(handles));

        Ok((app, handle))
    }

    pub async fn eth_subscribe(
        self: Arc<Self>,
        payload: JsonRpcRequest,
        subscription_count: &AtomicUsize,
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

                        let msg = Message::Text(serde_json::to_string(&msg).unwrap());

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

                        let msg = Message::Text(serde_json::to_string(&msg).unwrap());

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

                        let msg = Message::Text(serde_json::to_string(&msg).unwrap());

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
    #[instrument(skip_all)]
    pub async fn proxy_web3_rpc(
        &self,
        request: JsonRpcRequestEnum,
    ) -> anyhow::Result<JsonRpcForwardedResponseEnum> {
        trace!(?request, "proxy_web3_rpc");

        // even though we have timeouts on the requests to our backend providers,
        // we need a timeout for the incoming request so that retries don't run forever
        // TODO: take this as an optional argument. per user max? expiration time instead of duration?
        let max_time = Duration::from_secs(120);

        // TODO: instrument this with a unique id

        let response = match request {
            JsonRpcRequestEnum::Single(request) => JsonRpcForwardedResponseEnum::Single(
                timeout(max_time, self.proxy_web3_rpc_request(request)).await??,
            ),
            JsonRpcRequestEnum::Batch(requests) => JsonRpcForwardedResponseEnum::Batch(
                timeout(max_time, self.proxy_web3_rpc_requests(requests)).await??,
            ),
        };

        trace!(?response, "Forwarding");

        Ok(response)
    }

    async fn proxy_web3_rpc_requests(
        &self,
        requests: Vec<JsonRpcRequest>,
    ) -> anyhow::Result<Vec<JsonRpcForwardedResponse>> {
        // TODO: we should probably change ethers-rs to support this directly
        // we cut up the request and send to potentually different servers. this could be a problem.
        // if the client needs consistent blocks, they should specify instead of assume batches work on the same
        // TODO: is spawning here actually slower?
        let num_requests = requests.len();
        let responses = join_all(
            requests
                .into_iter()
                .map(|request| self.proxy_web3_rpc_request(request))
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

    pub async fn redis_conn(&self) -> anyhow::Result<PooledConnection<RedisConnectionManager>> {
        match self.redis_pool.as_ref() {
            None => Err(anyhow::anyhow!("no redis server configured")),
            Some(redis_pool) => {
                let redis_conn = redis_pool.get().await?;

                Ok(redis_conn)
            }
        }
    }

    async fn cached_response(
        &self,
        // TODO: accept a block hash here also?
        min_block_needed: Option<&U64>,
        request: &JsonRpcRequest,
    ) -> anyhow::Result<(
        CacheKey,
        Result<JsonRpcForwardedResponse, &ResponseLrcCache>,
    )> {
        // TODO: inspect the request to pick the right cache
        // TODO: https://github.com/ethereum/web3.py/blob/master/web3/middleware/cache.py

        let request_block_hash = if let Some(min_block_needed) = min_block_needed {
            // TODO: maybe this should be on the app and not on balanced_rpcs
            self.balanced_rpcs.block_hash(min_block_needed).await?
        } else {
            // TODO: maybe this should be on the app and not on balanced_rpcs
            self.balanced_rpcs
                .head_block_hash()
                .context("no servers in sync")?
        };

        // TODO: better key? benchmark this
        let key = (
            request_block_hash,
            request.method.clone(),
            request.params.clone().map(|x| x.to_string()),
        );

        if let Some(response) = self.response_cache.read().get(&key) {
            // TODO: emit a stat
            trace!(?request.method, "cache hit!");

            // TODO: can we make references work? maybe put them in an Arc?
            return Ok((key, Ok(response.to_owned())));
        } else {
            // TODO: emit a stat
            trace!(?request.method, "cache miss!");
        }

        let cache = &self.response_cache;

        Ok((key, Err(cache)))
    }

    async fn proxy_web3_rpc_request(
        &self,
        mut request: JsonRpcRequest,
    ) -> anyhow::Result<JsonRpcForwardedResponse> {
        trace!("Received request: {:?}", request);

        // TODO: if eth_chainId or net_version, serve those without querying the backend

        // TODO: how much should we retry? probably with a timeout and not with a count like this
        // TODO: think more about this loop.
        // // TODO: add more to this span such as
        let span = info_span!("rpc_request");
        // let _enter = span.enter(); // DO NOT ENTER! we can't use enter across awaits! (clippy lint soon)

        let partial_response: serde_json::Value = match request.method.as_ref() {
            // lots of commands are blocked
            "admin_addPeer"
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
            | "shh_version" => {
                // TODO: client error stat
                // TODO: proper error code
                return Err(anyhow::anyhow!("unsupported"));
            }
            // TODO: implement these commands
            "eth_getFilterChanges"
            | "eth_getFilterLogs"
            | "eth_newBlockFilter"
            | "eth_newFilter"
            | "eth_newPendingTransactionFilter"
            | "eth_uninstallFilter" => {
                // TODO: unsupported command stat
                return Err(anyhow::anyhow!("not yet implemented"));
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
                        return Err(anyhow::anyhow!("no servers synced"));
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
                return self
                    .private_rpcs
                    .try_send_all_upstream_servers(request, None)
                    .instrument(span)
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
                            return Err(anyhow::anyhow!("invalid request"));
                        }

                        let param = Bytes::from_str(params[0].as_str().unwrap())?;

                        let hash = H256::from(keccak256(param));

                        json!(hash)
                    }
                    _ => return Err(anyhow::anyhow!("invalid request")),
                }
            }

            // TODO: web3_sha3?
            // anything else gets sent to backend rpcs and cached
            method => {
                // emit stats

                let head_block_number = self
                    .balanced_rpcs
                    .head_block_num()
                    .context("no servers synced")?;

                // we do this check before checking caches because it might modify the request params
                // TODO: add a stat for archive vs full since they should probably cost different
                let min_block_needed =
                    block_needed(method, request.params.as_mut(), head_block_number);

                let min_block_needed = min_block_needed.as_ref();

                trace!(?min_block_needed, ?method);

                // TODO: emit a stat on error. maybe with .map_err?
                let (cache_key, cache_result) =
                    self.cached_response(min_block_needed, &request).await?;

                let response_cache = match cache_result {
                    Ok(response) => {
                        let _ = self.active_requests.remove(&cache_key);

                        // TODO: if the response is cached, should it count less against the account's costs?

                        return Ok(response);
                    }
                    Err(response_cache) => response_cache,
                };

                // check if this request is already in flight
                // TODO: move this logic into an IncomingRequestHandler (ActiveRequestHandler has an rpc, but this won't)
                let (incoming_tx, incoming_rx) = watch::channel(true);
                let mut other_incoming_rx = None;
                match self.active_requests.entry(cache_key.clone()) {
                    DashMapEntry::Occupied(entry) => {
                        other_incoming_rx = Some(entry.get().clone());
                    }
                    DashMapEntry::Vacant(entry) => {
                        entry.insert(incoming_rx);
                    }
                }

                if let Some(mut other_incoming_rx) = other_incoming_rx {
                    // wait for the other request to finish. it might have finished successfully or with an error
                    trace!("{:?} waiting on in-flight request", request);

                    let _ = other_incoming_rx.changed().await;

                    // now that we've waited, lets check the cache again
                    if let Some(cached) = response_cache.read().get(&cache_key) {
                        let _ = self.active_requests.remove(&cache_key);
                        let _ = incoming_tx.send(false);

                        // TODO: emit a stat
                        trace!(
                            "{:?} cache hit after waiting for in-flight request!",
                            request
                        );

                        return Ok(cached.to_owned());
                    } else {
                        // TODO: emit a stat
                        trace!(
                            "{:?} cache miss after waiting for in-flight request!",
                            request
                        );
                    }
                }

                let response = match method {
                    "temporarily disabled" => {
                        // "eth_getTransactionByHash" | "eth_getTransactionReceipt" => {
                        // TODO: try_send_all serially with retries instead of parallel
                        self.private_rpcs
                            .try_send_all_upstream_servers(request, min_block_needed)
                            .await?
                    }
                    _ => {
                        // TODO: retries?
                        self.balanced_rpcs
                            .try_send_best_upstream_server(request, min_block_needed)
                            .await?
                    }
                };

                // TODO: move this caching outside this match and cache some of the other responses?
                // TODO: cache the warp::reply to save us serializing every time?
                {
                    let mut response_cache = response_cache.write();

                    if response_cache.insert(cache_key.clone(), response.clone()) {
                    } else {
                        // TODO: emit a stat instead? what else should be in the log
                        trace!(?cache_key, "value too large for caching");
                    }
                }

                let _ = self.active_requests.remove(&cache_key);
                let _ = incoming_tx.send(false);

                return Ok(response);
            }
        };

        let response = JsonRpcForwardedResponse::from_value(partial_response, request.id);

        Ok(response)
    }
}

impl fmt::Debug for Web3ProxyApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("Web3ProxyApp").finish_non_exhaustive()
    }
}
