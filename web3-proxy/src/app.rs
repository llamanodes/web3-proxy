use axum::extract::ws::Message;
use dashmap::mapref::entry::Entry as DashMapEntry;
use dashmap::DashMap;
use ethers::prelude::{Address, Transaction};
use ethers::prelude::{Block, TxHash, H256};
use futures::future::Abortable;
use futures::future::{join_all, AbortHandle};
use futures::stream::FuturesUnordered;
use futures::stream::StreamExt;
use futures::Future;
use linkedhashmap::LinkedHashMap;
use parking_lot::RwLock;
use redis_cell_client::bb8::ErrorSink;
use redis_cell_client::{bb8, RedisCellClient, RedisConnectionManager};
use serde_json::json;
use std::fmt;
use std::pin::Pin;
use std::sync::atomic::{self, AtomicUsize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_stream::wrappers::{BroadcastStream, WatchStream};
use tracing::{info, info_span, instrument, trace, warn, Instrument};

use crate::bb8_helpers;
use crate::config::AppConfig;
use crate::connections::Web3Connections;
use crate::jsonrpc::JsonRpcForwardedResponse;
use crate::jsonrpc::JsonRpcForwardedResponseEnum;
use crate::jsonrpc::JsonRpcRequest;
use crate::jsonrpc::JsonRpcRequestEnum;

// TODO: make this customizable?
static APP_USER_AGENT: &str = concat!(
    "satoshiandkin/",
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
);

// TODO: put this in config? what size should we do? probably should structure this to be a size in MB
const RESPONSE_CACHE_CAP: usize = 1024;

// block hash, method, params
type CacheKey = (H256, String, Option<String>);

type ResponseLrcCache = RwLock<LinkedHashMap<CacheKey, JsonRpcForwardedResponse>>;

type ActiveRequestsMap = DashMap<CacheKey, watch::Receiver<bool>>;

pub type AnyhowJoinHandle<T> = JoinHandle<anyhow::Result<T>>;

pub async fn flatten_handle<T>(handle: AnyhowJoinHandle<T>) -> anyhow::Result<T> {
    match handle.await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(err)) => Err(err),
        Err(err) => Err(err.into()),
    }
}

pub async fn flatten_handles<T>(
    mut handles: FuturesUnordered<AnyhowJoinHandle<T>>,
) -> anyhow::Result<()> {
    while let Some(x) = handles.next().await {
        match x {
            Err(e) => return Err(e.into()),
            Ok(Err(e)) => return Err(e),
            Ok(Ok(_)) => {}
        }
    }

    Ok(())
}

fn is_archive_needed(method: &str, params: Option<&mut serde_json::Value>) -> bool {
    let methods_that_may_need_archive = [
        "eth_call",
        "eth_getBalance",
        "eth_getCode",
        "eth_getLogs",
        "eth_getStorageAt",
        "eth_getTransactionCount",
        "eth_getTransactionByBlockHashAndIndex",
        "eth_getTransactionByBlockNumberAndIndex",
        "eth_getTransactionReceipt",
        "eth_getUncleByBlockHashAndIndex",
        "eth_getUncleByBlockNumberAndIndex",
    ];

    if !methods_that_may_need_archive.contains(&method) {
        // this method is not known to require an archive node
        return false;
    }

    // TODO: find the given block number in params

    // TODO: if its "latest" (or not given), modify params to have the latest block. return false

    // TODO: if its "pending", do something special? return false

    // TODO: we probably need a list of recent hashes/heights. if specified block is recent, return false

    // this command needs an archive server
    true
}

// TODO: think more about TxState. d
#[derive(Clone)]
pub enum TxState {
    Pending(Transaction),
    Confirmed(Transaction),
    Orphaned(Transaction),
}

/// The application
// TODO: this debug impl is way too verbose. make something smaller
// TODO: if Web3ProxyApp is always in an Arc, i think we can avoid having at least some of these internal things in arcs
// TODO: i'm sure this is more arcs than necessary, but spawning futures makes references hard
pub struct Web3ProxyApp {
    /// Send requests to the best server available
    balanced_rpcs: Arc<Web3Connections>,
    /// Send private requests (like eth_sendRawTransaction) to all these servers
    private_rpcs: Arc<Web3Connections>,
    incoming_requests: ActiveRequestsMap,
    response_cache: ResponseLrcCache,
    // don't drop this or the sender will stop working
    // TODO: broadcast channel instead?
    head_block_receiver: watch::Receiver<Block<TxHash>>,
    pending_tx_sender: broadcast::Sender<TxState>,
    pending_transactions: Arc<DashMap<TxHash, TxState>>,
    public_rate_limiter: Option<RedisCellClient>,
}

impl fmt::Debug for Web3ProxyApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("Web3ProxyApp").finish_non_exhaustive()
    }
}

impl Web3ProxyApp {
    pub fn get_pending_transactions(&self) -> &DashMap<TxHash, TxState> {
        &self.pending_transactions
    }

    pub fn get_public_rate_limiter(&self) -> Option<&RedisCellClient> {
        self.public_rate_limiter.as_ref()
    }

    // TODO: should we just take the rpc config as the only arg instead?
    pub async fn spawn(
        app_config: AppConfig,
        num_workers: usize,
    ) -> anyhow::Result<(
        Arc<Web3ProxyApp>,
        Pin<Box<dyn Future<Output = anyhow::Result<()>>>>,
    )> {
        let balanced_rpcs = app_config.balanced_rpcs.into_values().collect();

        let private_rpcs = if let Some(private_rpcs) = app_config.private_rpcs {
            private_rpcs.into_values().collect()
        } else {
            vec![]
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

        let redis_client_pool = match app_config.shared.rate_limit_redis {
            Some(redis_address) => {
                info!("Connecting to redis on {}", redis_address);

                let manager = RedisConnectionManager::new(redis_address)?;

                let min_size = num_workers as u32;
                let max_size = min_size * 4;

                // TODO: min_idle?
                // TODO: set max_size based on max expected concurrent connections? set based on num_workers?
                let builder = bb8::Pool::builder()
                    .error_sink(bb8_helpers::RedisErrorSink.boxed_clone())
                    .min_idle(Some(min_size))
                    .max_size(max_size);

                let pool = builder.build(manager).await?;

                Some(pool)
            }
            None => {
                info!("No redis address");
                None
            }
        };

        let (head_block_sender, head_block_receiver) = watch::channel(Block::default());
        // TODO: will one receiver lagging be okay? how big should this be?
        let (pending_tx_sender, pending_tx_receiver) = broadcast::channel(16);

        let pending_transactions = Arc::new(DashMap::new());

        // TODO: don't drop the pending_tx_receiver. instead, read it to mark transactions as "seen". once seen, we won't re-send them
        // TODO: once a transaction is "Confirmed" we remove it from the map. this should prevent major memory leaks.
        // TODO: we should still have some sort of expiration or maximum size limit for the map

        // TODO: attach context to this error
        let (balanced_rpcs, balanced_handle) = Web3Connections::spawn(
            app_config.shared.chain_id,
            balanced_rpcs,
            http_client.as_ref(),
            redis_client_pool.as_ref(),
            Some(head_block_sender),
            Some(pending_tx_sender.clone()),
            pending_transactions.clone(),
        )
        .await?;

        handles.push(balanced_handle);

        let private_rpcs = if private_rpcs.is_empty() {
            warn!("No private relays configured. Any transactions will be broadcast to the public mempool!");
            balanced_rpcs.clone()
        } else {
            // TODO: attach context to this error
            let (private_rpcs, private_handle) = Web3Connections::spawn(
                app_config.shared.chain_id,
                private_rpcs,
                http_client.as_ref(),
                redis_client_pool.as_ref(),
                // subscribing to new heads here won't work well
                None,
                // TODO: subscribe to pending transactions on the private rpcs?
                Some(pending_tx_sender.clone()),
                pending_transactions.clone(),
            )
            .await?;

            handles.push(private_handle);

            private_rpcs
        };

        // TODO: use this? it could listen for confirmed transactions and then clear pending_transactions, but the head_block_sender is doing that
        drop(pending_tx_receiver);

        // TODO: how much should we allow?
        let public_max_burst = app_config.shared.public_rate_limit_per_minute / 3;

        let public_rate_limiter = if app_config.shared.public_rate_limit_per_minute == 0 {
            None
        } else {
            redis_client_pool.as_ref().map(|redis_client_pool| {
                RedisCellClient::new(
                    redis_client_pool.clone(),
                    "ip".to_string(),
                    public_max_burst,
                    app_config.shared.public_rate_limit_per_minute,
                    60,
                )
            })
        };

        let app = Self {
            balanced_rpcs,
            private_rpcs,
            incoming_requests: Default::default(),
            response_cache: Default::default(),
            head_block_receiver,
            pending_tx_sender,
            pending_transactions,
            public_rate_limiter,
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
        let subscription_id = format!("{:#x}", subscription_id);

        // save the id so we can use it in the response
        let id = payload.id.clone();

        // TODO: calling json! on every request is probably not fast.
        match payload.params {
            Some(x) if x == json!(["newHeads"]) => {
                let head_block_receiver = self.head_block_receiver.clone();

                let subscription_id = subscription_id.clone();

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
                                "result": new_head,
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
            Some(x) if x == json!(["newPendingTransactions"]) => {
                let pending_tx_receiver = self.pending_tx_sender.subscribe();

                let subscription_id = subscription_id.clone();

                let mut pending_tx_receiver = Abortable::new(
                    BroadcastStream::new(pending_tx_receiver),
                    subscription_registration,
                );

                trace!(?subscription_id, "pending transactions subscription");
                tokio::spawn(async move {
                    while let Some(Ok(new_tx_state)) = pending_tx_receiver.next().await {
                        let new_tx = match new_tx_state {
                            TxState::Pending(tx) => tx,
                            TxState::Confirmed(..) => continue,
                            TxState::Orphaned(tx) => tx,
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

                let subscription_id = subscription_id.clone();

                let mut pending_tx_receiver = Abortable::new(
                    BroadcastStream::new(pending_tx_receiver),
                    subscription_registration,
                );

                trace!(?subscription_id, "pending transactions subscription");

                // TODO: do something with this handle?
                tokio::spawn(async move {
                    while let Some(Ok(new_tx_state)) = pending_tx_receiver.next().await {
                        let new_tx = match new_tx_state {
                            TxState::Pending(tx) => tx,
                            TxState::Confirmed(..) => continue,
                            TxState::Orphaned(tx) => tx,
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

                let subscription_id = subscription_id.clone();

                trace!(?subscription_id, "pending transactions subscription");

                // TODO: do something with this handle?
                tokio::spawn(async move {
                    while let Some(Ok(new_tx_state)) = pending_tx_receiver.next().await {
                        let new_tx = match new_tx_state {
                            TxState::Pending(tx) => tx,
                            TxState::Confirmed(..) => continue,
                            TxState::Orphaned(tx) => tx,
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

        let response = JsonRpcForwardedResponse::from_string(subscription_id, id);

        // TODO: make a `SubscriptonHandle(AbortHandle, JoinHandle)` struct?

        Ok((subscription_abort_handle, response))
    }

    pub fn get_balanced_rpcs(&self) -> &Web3Connections {
        &self.balanced_rpcs
    }

    pub fn get_private_rpcs(&self) -> &Web3Connections {
        &self.private_rpcs
    }

    pub fn get_active_requests(&self) -> &ActiveRequestsMap {
        &self.incoming_requests
    }

    /// send the request to the approriate RPCs
    /// TODO: dry this up
    #[instrument(skip_all)]
    pub async fn proxy_web3_rpc(
        &self,
        request: JsonRpcRequestEnum,
    ) -> anyhow::Result<JsonRpcForwardedResponseEnum> {
        // TODO: i don't always see this in the logs. why?
        trace!("Received request: {:?}", request);

        // even though we have timeouts on the requests to our backend providers,
        // we need a timeout for the incoming request so that delays from
        let max_time = Duration::from_secs(60);

        let response = match request {
            JsonRpcRequestEnum::Single(request) => JsonRpcForwardedResponseEnum::Single(
                timeout(max_time, self.proxy_web3_rpc_request(request)).await??,
            ),
            JsonRpcRequestEnum::Batch(requests) => JsonRpcForwardedResponseEnum::Batch(
                timeout(max_time, self.proxy_web3_rpc_requests(requests)).await??,
            ),
        };

        // TODO: i don't always see this in the logs. why?
        trace!("Forwarding response: {:?}", response);

        Ok(response)
    }

    // #[instrument(skip_all)]
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

    fn get_cached_response(
        &self,
        request: &JsonRpcRequest,
    ) -> (
        CacheKey,
        Result<JsonRpcForwardedResponse, &ResponseLrcCache>,
    ) {
        // TODO: inspect the request to pick the right cache
        // TODO: https://github.com/ethereum/web3.py/blob/master/web3/middleware/cache.py

        // TODO: Some requests should skip caching on the head_block_hash
        let head_block_hash = self.balanced_rpcs.get_head_block_hash();

        // TODO: better key? benchmark this
        let key = (
            head_block_hash,
            request.method.clone(),
            request.params.clone().map(|x| x.to_string()),
        );

        if let Some(response) = self.response_cache.read().get(&key) {
            // TODO: emit a stat
            trace!("{:?} cache hit!", request);

            // TODO: can we make references work? maybe put them in an Arc?
            return (key, Ok(response.to_owned()));
        } else {
            // TODO: emit a stat
            trace!("{:?} cache miss!", request);
        }

        // TODO: multiple caches. if head_block_hash is None, have a persistent cache (disk backed?)
        let cache = &self.response_cache;

        (key, Err(cache))
    }

    // #[instrument(skip_all)]
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
        match &request.method[..] {
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
                // TODO: proper error code
                Err(anyhow::anyhow!("unsupported"))
            }
            // TODO: implement these commands
            "eth_getFilterChanges"
            | "eth_getFilterLogs"
            | "eth_newBlockFilter"
            | "eth_newFilter"
            | "eth_newPendingTransactionFilter"
            | "eth_uninstallFilter" => Err(anyhow::anyhow!("not yet implemented")),
            // some commands can use local data or caches
            "eth_accounts" => {
                let partial_response = serde_json::Value::Array(vec![]);

                let response = JsonRpcForwardedResponse::from_value(partial_response, request.id);

                Ok(response)
            }
            "eth_blockNumber" => {
                let head_block_number = self.balanced_rpcs.get_head_block_num();

                if head_block_number == 0 {
                    return Err(anyhow::anyhow!("no servers synced"));
                }

                // TODO: this seems pretty common. make a helper?
                let partial_response = format!("{:x}", head_block_number);

                let response = JsonRpcForwardedResponse::from_string(partial_response, request.id);

                Ok(response)
            }
            // TODO: eth_callBundle (https://docs.flashbots.net/flashbots-auction/searchers/advanced/rpc-endpoint#eth_callbundle)
            // TODO: eth_cancelPrivateTransaction (https://docs.flashbots.net/flashbots-auction/searchers/advanced/rpc-endpoint#eth_cancelprivatetransaction, but maybe just reject)
            // TODO: eth_sendPrivateTransaction (https://docs.flashbots.net/flashbots-auction/searchers/advanced/rpc-endpoint#eth_sendprivatetransaction)
            "eth_coinbase" => {
                // no need for serving coinbase. we could return a per-user payment address here, but then we might leak that to dapps
                let partial_response = json!(Address::zero());

                let response = JsonRpcForwardedResponse::from_value(partial_response, request.id);

                Ok(response)
            }
            // TODO: eth_estimateGas using anvil?
            // TODO: eth_gasPrice that does awesome magic to predict the future
            // TODO: eth_getBlockByHash from caches
            // TODO: eth_getBlockByNumber from caches
            // TODO: eth_getBlockTransactionCountByHash from caches
            // TODO: eth_getBlockTransactionCountByNumber from caches
            // TODO: eth_getUncleCountByBlockHash from caches
            // TODO: eth_getUncleCountByBlockNumber from caches
            "eth_hashrate" => {
                let partial_response = json!("0x0");

                let response = JsonRpcForwardedResponse::from_value(partial_response, request.id);

                Ok(response)
            }
            "eth_mining" => {
                let partial_response = json!(false);

                let response = JsonRpcForwardedResponse::from_value(partial_response, request.id);

                Ok(response)
            }
            // TODO: eth_sendBundle (flashbots command)
            // broadcast transactions to all private rpcs at once
            "eth_sendRawTransaction" => {
                self.private_rpcs
                    .try_send_all_upstream_servers(request, false)
                    .instrument(span)
                    .await
            }
            "eth_syncing" => {
                // TODO: return a real response if all backends are syncing or if no servers in sync
                let partial_response = serde_json::Value::Bool(false);

                let response = JsonRpcForwardedResponse::from_value(partial_response, request.id);

                Ok(response)
            }
            "net_listening" => {
                // TODO: only if there are some backends on balanced_rpcs?
                let partial_response = serde_json::Value::Bool(true);

                let response = JsonRpcForwardedResponse::from_value(partial_response, request.id);

                Ok(response)
            }
            "net_peerCount" => {
                let partial_response = serde_json::Value::String(format!(
                    "{:x}",
                    self.balanced_rpcs.num_synced_rpcs()
                ));

                let response = JsonRpcForwardedResponse::from_value(partial_response, request.id);

                Ok(response)
            }
            "web3_clientVersion" => {
                // TODO: return a real response if all backends are syncing or if no servers in sync
                let partial_response = serde_json::Value::String(APP_USER_AGENT.to_string());

                let response = JsonRpcForwardedResponse::from_value(partial_response, request.id);

                Ok(response)
            }
            // TODO: web3_sha3?
            method => {
                // everything else is relayed to a backend
                // this is not a private transaction

                // we do this check before checking caches because it might modify the request params
                let archive_needed = is_archive_needed(method, request.params.as_mut());

                let (cache_key, response_cache) = match self.get_cached_response(&request) {
                    (cache_key, Ok(response)) => {
                        let _ = self.incoming_requests.remove(&cache_key);

                        return Ok(response);
                    }
                    (cache_key, Err(response_cache)) => (cache_key, response_cache),
                };

                // check if this request is already in flight
                // TODO: move this logic into an IncomingRequestHandler (ActiveRequestHandler has an rpc, but this won't)
                let (incoming_tx, incoming_rx) = watch::channel(true);
                let mut other_incoming_rx = None;
                match self.incoming_requests.entry(cache_key.clone()) {
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
                        let _ = self.incoming_requests.remove(&cache_key);
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
                    "eth_getTransactionByHash" | "eth_getTransactionReceipt" => {
                        // TODO: try_send_all serially with retries instead of parallel
                        self.private_rpcs
                            .try_send_all_upstream_servers(request, archive_needed)
                            .await?
                    }
                    _ => {
                        // TODO: retries?
                        self.balanced_rpcs
                            .try_send_best_upstream_server(request, archive_needed)
                            .await?
                    }
                };

                {
                    let mut response_cache = response_cache.write();

                    // TODO: cache the warp::reply to save us serializing every time?
                    response_cache.insert(cache_key.clone(), response.clone());

                    // TODO: instead of checking length, check size in bytes
                    if response_cache.len() >= RESPONSE_CACHE_CAP {
                        // TODO: this isn't an LRU. it's a "least recently created". does that have a fancy name? should we make it an lru? these caches only live for one block
                        response_cache.pop_front();
                    }
                }

                let _ = self.incoming_requests.remove(&cache_key);
                let _ = incoming_tx.send(false);

                Ok(response)
            }
        }
    }
}
