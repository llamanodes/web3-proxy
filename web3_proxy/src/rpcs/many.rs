//! Load balanced communication with a group of web3 rpc providers
use super::blockchain::{BlocksByHashCache, BlocksByNumberCache, Web3ProxyBlock};
use super::consensus::{RankedRpcs, ShouldWaitForBlock};
use super::one::Web3Rpc;
use super::request::{OpenRequestHandle, OpenRequestResult, RequestErrorHandler};
use crate::app::{flatten_handle, Web3ProxyApp, Web3ProxyJoinHandle};
use crate::config::{average_block_interval, BlockAndRpc, TxHashAndRpc, Web3RpcConfig};
use crate::errors::{Web3ProxyError, Web3ProxyResult};
use crate::frontend::authorization::{Authorization, RequestMetadata};
use crate::frontend::rpc_proxy_ws::ProxyMode;
use crate::frontend::status::MokaCacheSerializer;
use crate::jsonrpc::{JsonRpcErrorData, JsonRpcParams, JsonRpcResultData};
use crate::rpcs::transactions::TxStatus;
use counter::Counter;
use derive_more::From;
use ethers::prelude::{ProviderError, TxHash, U64};
use futures::future::try_join_all;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use hashbrown::HashMap;
use itertools::Itertools;
use migration::sea_orm::DatabaseConnection;
use moka::future::{Cache, CacheBuilder};
use parking_lot::RwLock;
use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;
use serde_json::json;
use serde_json::value::RawValue;
use std::cmp::min_by_key;
use std::fmt::{self, Display};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::select;
use tokio::sync::{broadcast, watch};
use tokio::time::{sleep, sleep_until, Duration, Instant};
use tracing::{debug, error, info, trace, warn};

/// A collection of web3 connections. Sends requests either the current best server or all servers.
#[derive(From)]
pub struct Web3Rpcs {
    /// TODO: this should be a Cow
    pub(crate) name: String,
    pub(crate) chain_id: u64,
    /// if watch_consensus_head_sender is some, Web3Rpc inside self will send blocks here when they get them
    pub(crate) block_sender: flume::Sender<(Option<Web3ProxyBlock>, Arc<Web3Rpc>)>,
    /// any requests will be forwarded to one (or more) of these connections
    /// TODO: hopefully this not being an async lock will be okay. if you need it across awaits, clone the arc
    pub(crate) by_name: RwLock<HashMap<String, Arc<Web3Rpc>>>,
    /// all providers with the same consensus head block. won't update if there is no `self.watch_consensus_head_sender`
    /// TODO: document that this is a watch sender and not a broadcast! if things get busy, blocks might get missed
    /// TODO: why is watch_consensus_head_sender in an Option, but this one isn't?
    /// Geth's subscriptions have the same potential for skipping blocks.
    pub(crate) watch_ranked_rpcs: watch::Sender<Option<Arc<RankedRpcs>>>,
    /// this head receiver makes it easy to wait until there is a new block
    pub(super) watch_head_block: Option<watch::Sender<Option<Web3ProxyBlock>>>,
    /// keep track of transactions that we have sent through subscriptions
    pub(super) pending_transaction_cache: Cache<TxHash, TxStatus>,
    pub(super) pending_tx_id_receiver: flume::Receiver<TxHashAndRpc>,
    pub(super) pending_tx_id_sender: flume::Sender<TxHashAndRpc>,
    /// TODO: this map is going to grow forever unless we do some sort of pruning. maybe store pruned in redis?
    /// all blocks, including orphans
    pub(super) blocks_by_hash: BlocksByHashCache,
    /// blocks on the heaviest chain
    pub(super) blocks_by_number: BlocksByNumberCache,
    /// the number of rpcs required to agree on consensus for the head block (thundering herd protection)
    pub(super) min_synced_rpcs: usize,
    /// the soft limit required to agree on consensus for the head block. (thundering herd protection)
    pub(super) min_sum_soft_limit: u32,
    /// how far behind the highest known block height we can be before we stop serving requests
    pub(super) max_head_block_lag: U64,
    /// how old our consensus head block we can be before we stop serving requests
    /// calculated based on max_head_block_lag and averge block times
    pub(super) max_head_block_age: Duration,
}

impl Web3Rpcs {
    /// Spawn durable connections to multiple Web3 providers.
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn(
        chain_id: u64,
        db_conn: Option<DatabaseConnection>,
        max_head_block_lag: Option<U64>,
        min_head_rpcs: usize,
        min_sum_soft_limit: u32,
        name: String,
        pending_transaction_cache: Cache<TxHash, TxStatus>,
        pending_tx_sender: Option<broadcast::Sender<TxStatus>>,
        watch_consensus_head_sender: Option<watch::Sender<Option<Web3ProxyBlock>>>,
    ) -> anyhow::Result<(
        Arc<Self>,
        Web3ProxyJoinHandle<()>,
        watch::Receiver<Option<Arc<RankedRpcs>>>,
    )> {
        let (pending_tx_id_sender, pending_tx_id_receiver) = flume::unbounded();
        let (block_sender, block_receiver) = flume::unbounded::<BlockAndRpc>();

        // these blocks don't have full transactions, but they do have rather variable amounts of transaction hashes
        // TODO: actual weighter on this
        // TODO: time_to_idle instead?
        let blocks_by_hash: BlocksByHashCache = CacheBuilder::new(1_000)
            .name("blocks_by_hash")
            .time_to_idle(Duration::from_secs(30 * 60))
            .build();

        // all block numbers are the same size, so no need for weigher
        // TODO: limits from config
        // TODO: time_to_idle instead?
        let blocks_by_number = CacheBuilder::new(1_000)
            .name("blocks_by_number")
            .time_to_idle(Duration::from_secs(30 * 60))
            .build();

        let (watch_consensus_rpcs_sender, consensus_connections_watcher) =
            watch::channel(Default::default());

        // by_name starts empty. self.apply_server_configs will add to it
        let by_name = RwLock::new(HashMap::new());

        let max_head_block_lag = max_head_block_lag.unwrap_or(5.into());

        let max_head_block_age =
            average_block_interval(chain_id).mul_f32((max_head_block_lag.as_u64() * 10) as f32);

        let connections = Arc::new(Self {
            block_sender,
            blocks_by_hash,
            blocks_by_number,
            by_name,
            chain_id,
            max_head_block_age,
            max_head_block_lag,
            min_synced_rpcs: min_head_rpcs,
            min_sum_soft_limit,
            name,
            pending_transaction_cache,
            pending_tx_id_receiver,
            pending_tx_id_sender,
            watch_head_block: watch_consensus_head_sender,
            watch_ranked_rpcs: watch_consensus_rpcs_sender,
        });

        let authorization = Arc::new(Authorization::internal(db_conn)?);

        let handle = {
            let connections = connections.clone();

            tokio::spawn(connections.subscribe(authorization, block_receiver, pending_tx_sender))
        };

        Ok((connections, handle, consensus_connections_watcher))
    }

    /// update the rpcs in this group
    pub async fn apply_server_configs(
        &self,
        app: &Web3ProxyApp,
        rpc_configs: HashMap<String, Web3RpcConfig>,
    ) -> Web3ProxyResult<()> {
        // safety checks
        if rpc_configs.len() < app.config.min_synced_rpcs {
            // TODO: don't count disabled servers!
            // TODO: include if this is balanced, private, or 4337
            warn!(
                "Only {}/{} rpcs! Add more rpcs or reduce min_synced_rpcs.",
                rpc_configs.len(),
                app.config.min_synced_rpcs
            );
            return Ok(());
        }

        // safety check on sum soft limit
        // TODO: will need to think about this more once sum_soft_limit is dynamic
        let sum_soft_limit = rpc_configs.values().fold(0, |acc, x| acc + x.soft_limit);

        // TODO: require a buffer?
        if sum_soft_limit < self.min_sum_soft_limit {
            return Err(Web3ProxyError::NotEnoughSoftLimit {
                available: sum_soft_limit,
                needed: self.min_sum_soft_limit,
            });
        }

        let chain_id = app.config.chain_id;

        let block_interval = average_block_interval(chain_id);

        // turn configs into connections (in parallel)
        let mut spawn_handles: FuturesUnordered<_> = rpc_configs
            .into_iter()
            .filter_map(|(server_name, server_config)| {
                if server_config.disabled {
                    info!("{} is disabled", server_name);
                    return None;
                }

                let db_conn = app.db_conn().ok().cloned();
                let http_client = app.http_client.clone();
                let vredis_pool = app.vredis_pool.clone();

                let block_sender = if self.watch_head_block.is_some() {
                    Some(self.block_sender.clone())
                } else {
                    None
                };

                let pending_tx_id_sender = Some(self.pending_tx_id_sender.clone());
                let blocks_by_hash_cache = self.blocks_by_hash.clone();

                debug!("spawning tasks for {}", server_name);

                let handle = tokio::spawn(server_config.spawn(
                    server_name,
                    db_conn,
                    vredis_pool,
                    chain_id,
                    block_interval,
                    http_client,
                    blocks_by_hash_cache,
                    block_sender,
                    pending_tx_id_sender,
                ));

                Some(handle)
            })
            .collect();

        while let Some(x) = spawn_handles.next().await {
            match x {
                Ok(Ok((new_rpc, _handle))) => {
                    // web3 connection worked

                    let old_rpc = self.by_name.read().get(&new_rpc.name).map(Arc::clone);

                    // clean up the old rpc if it exists
                    if let Some(old_rpc) = old_rpc {
                        trace!("old_rpc: {}", old_rpc);

                        // if the old rpc was synced, wait for the new one to sync
                        if old_rpc.head_block.as_ref().unwrap().borrow().is_some() {
                            let mut new_head_receiver =
                                new_rpc.head_block.as_ref().unwrap().subscribe();
                            trace!("waiting for new {} connection to sync", new_rpc);

                            // TODO: maximum wait time
                            while new_head_receiver.borrow_and_update().is_none() {
                                if new_head_receiver.changed().await.is_err() {
                                    break;
                                };
                            }
                        }

                        // new rpc is synced (or old one was not synced). update the local map
                        // make sure that any new requests use the new connection
                        self.by_name.write().insert(new_rpc.name.clone(), new_rpc);

                        // tell the old rpc to disconnect
                        if let Some(ref disconnect_sender) = old_rpc.disconnect_watch {
                            trace!("telling {} to disconnect", old_rpc);
                            disconnect_sender.send_replace(true);
                        }
                    } else {
                        self.by_name.write().insert(new_rpc.name.clone(), new_rpc);
                    }
                }
                Ok(Err(err)) => {
                    // if we got an error here, the app can continue on
                    // TODO: include context about which connection failed
                    // TODO: retry automatically
                    error!("Unable to create connection. err={:?}", err);
                }
                Err(err) => {
                    // something actually bad happened. exit with an error
                    return Err(err.into());
                }
            }
        }

        let num_rpcs = self.len();

        if num_rpcs < self.min_synced_rpcs {
            return Err(Web3ProxyError::NotEnoughRpcs {
                num_known: num_rpcs,
                min_head_rpcs: self.min_synced_rpcs,
            });
        }

        Ok(())
    }

    pub fn get(&self, conn_name: &str) -> Option<Arc<Web3Rpc>> {
        self.by_name.read().get(conn_name).cloned()
    }

    pub fn len(&self) -> usize {
        self.by_name.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.read().is_empty()
    }

    /// TODO: rename to be consistent between "head" and "synced"
    pub fn min_head_rpcs(&self) -> usize {
        self.min_synced_rpcs
    }

    /// subscribe to blocks and transactions from all the backend rpcs.
    /// blocks are processed by all the `Web3Rpc`s and then sent to the `block_receiver`
    /// transaction ids from all the `Web3Rpc`s are deduplicated and forwarded to `pending_tx_sender`
    async fn subscribe(
        self: Arc<Self>,
        authorization: Arc<Authorization>,
        block_receiver: flume::Receiver<BlockAndRpc>,
        pending_tx_sender: Option<broadcast::Sender<TxStatus>>,
    ) -> Web3ProxyResult<()> {
        let mut futures = vec![];

        // setup the transaction funnel
        // it skips any duplicates (unless they are being orphaned)
        // fetches new transactions from the notifying rpc
        // forwards new transacitons to pending_tx_receipt_sender
        if let Some(pending_tx_sender) = pending_tx_sender.clone() {
            let clone = self.clone();
            let authorization = authorization.clone();
            let pending_tx_id_receiver = self.pending_tx_id_receiver.clone();
            let handle = tokio::task::spawn(async move {
                // TODO: set up this future the same as the block funnel
                while let Ok((pending_tx_id, rpc)) = pending_tx_id_receiver.recv_async().await {
                    let f = clone.clone().process_incoming_tx_id(
                        authorization.clone(),
                        rpc,
                        pending_tx_id,
                        pending_tx_sender.clone(),
                    );
                    tokio::spawn(f);
                }

                Ok(())
            });

            futures.push(flatten_handle(handle));
        }

        // setup the block funnel
        if self.watch_head_block.is_some() {
            let connections = Arc::clone(&self);
            let pending_tx_sender = pending_tx_sender.clone();

            let handle = tokio::task::Builder::default()
                .name("process_incoming_blocks")
                .spawn(async move {
                    connections
                        .process_incoming_blocks(&authorization, block_receiver, pending_tx_sender)
                        .await
                })?;

            futures.push(flatten_handle(handle));
        }

        if futures.is_empty() {
            // no transaction or block subscriptions.

            let handle = tokio::task::Builder::default()
                .name("noop")
                .spawn(async move {
                    loop {
                        sleep(Duration::from_secs(600)).await;
                        // TODO: "every interval, do a health check or disconnect the rpc"
                    }
                })?;

            futures.push(flatten_handle(handle));
        }

        if let Err(e) = try_join_all(futures).await {
            error!("subscriptions over: {:?}", self);
            return Err(e);
        }

        info!("subscriptions over: {:?}", self);
        Ok(())
    }

    /// Send the same request to all the handles. Returning the most common success or most common error.
    /// TODO: option to return the fastest response and handles for all the others instead?
    pub async fn try_send_parallel_requests<P: JsonRpcParams>(
        &self,
        active_request_handles: Vec<OpenRequestHandle>,
        method: &str,
        params: &P,
        // TODO: remove this box once i figure out how to do the options
    ) -> Result<Box<RawValue>, ProviderError> {
        // TODO: if only 1 active_request_handles, do self.try_send_request?

        // TODO: iter stream
        let responses = active_request_handles
            .into_iter()
            .map(|active_request_handle| async move {
                let result: Result<Box<RawValue>, _> =
                    active_request_handle.request(method, &json!(&params)).await;
                result
            })
            .collect::<FuturesUnordered<_>>()
            .collect::<Vec<Result<Box<RawValue>, ProviderError>>>()
            .await;

        // TODO: Strings are not great keys, but we can't use RawValue or ProviderError as keys because they don't implement Hash or Eq
        let mut count_map: HashMap<String, _> = HashMap::new();
        let mut counts: Counter<String> = Counter::new();
        let mut any_ok_with_json_result = false;
        for partial_response in responses {
            if partial_response.is_ok() {
                any_ok_with_json_result = true;
            }

            // TODO: better key!
            let s = format!("{:?}", partial_response);

            if count_map.get(&s).is_none() {
                count_map.insert(s.clone(), partial_response);
            }

            counts.update([s].into_iter());
        }

        // return the most_common success if any. otherwise return the most_common error
        for (most_common, _) in counts.most_common_ordered() {
            let most_common = count_map
                .remove(&most_common)
                .expect("most_common key must exist");

            match most_common {
                Ok(x) => {
                    // return the most common success
                    return Ok(x);
                }
                Err(err) => {
                    if any_ok_with_json_result {
                        // the most common is an error, but there is an Ok in here somewhere. continue the loop to find it
                        continue;
                    }

                    return Err(err);
                }
            }
        }

        // TODO: what should we do if we get here? i don't think we will
        unimplemented!("this shouldn't be possible")
    }

    async fn _best_available_rpc(
        &self,
        authorization: &Arc<Authorization>,
        error_handler: Option<RequestErrorHandler>,
        potential_rpcs: &[Arc<Web3Rpc>],
        skip: &mut Vec<Arc<Web3Rpc>>,
    ) -> OpenRequestResult {
        let mut earliest_retry_at = None;

        for (rpc_a, rpc_b) in potential_rpcs.iter().circular_tuple_windows() {
            trace!("{} vs {}", rpc_a, rpc_b);
            // TODO: cached key to save a read lock
            // TODO: ties to the server with the smallest block_data_limit
            let faster_rpc = min_by_key(rpc_a, rpc_b, |x| x.weighted_peak_latency());
            trace!("winner: {}", faster_rpc);

            // add to the skip list in case this one fails
            skip.push(Arc::clone(faster_rpc));

            // just because it has lower latency doesn't mean we are sure to get a connection. there might be rate limits
            // TODO: what error_handler?
            match faster_rpc
                .try_request_handle(authorization, error_handler)
                .await
            {
                Ok(OpenRequestResult::Handle(handle)) => {
                    trace!("opened handle: {}", faster_rpc);
                    return OpenRequestResult::Handle(handle);
                }
                Ok(OpenRequestResult::RetryAt(retry_at)) => {
                    trace!(
                        "retry on {} @ {}",
                        faster_rpc,
                        retry_at.duration_since(Instant::now()).as_secs_f32()
                    );

                    if earliest_retry_at.is_none() {
                        earliest_retry_at = Some(retry_at);
                    } else {
                        earliest_retry_at = earliest_retry_at.min(Some(retry_at));
                    }
                }
                Ok(OpenRequestResult::NotReady) => {
                    // TODO: log a warning? emit a stat?
                    trace!("best_rpc not ready: {}", faster_rpc);
                }
                Err(err) => {
                    trace!("No request handle for {}. err={:?}", faster_rpc, err)
                }
            }
        }

        if let Some(retry_at) = earliest_retry_at {
            OpenRequestResult::RetryAt(retry_at)
        } else {
            OpenRequestResult::NotReady
        }
    }

    pub async fn wait_for_best_rpc(
        &self,
        request_metadata: Option<&Arc<RequestMetadata>>,
        skip_rpcs: &mut Vec<Arc<Web3Rpc>>,
        min_block_needed: Option<&U64>,
        max_block_needed: Option<&U64>,
        max_wait: Option<Duration>,
        error_handler: Option<RequestErrorHandler>,
    ) -> Web3ProxyResult<OpenRequestResult> {
        let start = Instant::now();

        let mut earliest_retry_at: Option<Instant> = None;

        // TODO: pass db_conn to the "default" authorization for revert logging
        let authorization = request_metadata
            .and_then(|x| x.authorization.clone())
            .unwrap_or_default();

        let mut watch_ranked_rpcs = self.watch_ranked_rpcs.subscribe();

        let mut potential_rpcs = Vec::new();

        loop {
            // TODO: need a change so that protected and 4337 rpcs set watch_consensus_rpcs on start
            let ranked_rpcs: Option<Arc<RankedRpcs>> =
                watch_ranked_rpcs.borrow_and_update().clone();

            // first check everything that is synced
            // even though we might be querying an old block that an unsynced server can handle,
            // it is best to not send queries to a syncing server. that slows down sync and can bloat erigon's disk usage.
            if let Some(ranked_rpcs) = ranked_rpcs {
                potential_rpcs.extend(
                    ranked_rpcs
                        .all()
                        .iter()
                        .filter(|rpc| {
                            ranked_rpcs.rpc_will_work_now(
                                skip_rpcs,
                                min_block_needed,
                                max_block_needed,
                                rpc,
                            )
                        })
                        .cloned(),
                );

                if potential_rpcs.len() >= self.min_synced_rpcs {
                    // we have enough potential rpcs. try to load balance
                    potential_rpcs.sort_by_cached_key(|x| {
                        x.shuffle_for_load_balancing_on(max_block_needed.copied())
                    });

                    match self
                        ._best_available_rpc(
                            &authorization,
                            error_handler,
                            &potential_rpcs,
                            skip_rpcs,
                        )
                        .await
                    {
                        OpenRequestResult::Handle(x) => return Ok(OpenRequestResult::Handle(x)),
                        OpenRequestResult::NotReady => {}
                        OpenRequestResult::RetryAt(retry_at) => {
                            if earliest_retry_at.is_none() {
                                earliest_retry_at = Some(retry_at);
                            } else {
                                earliest_retry_at = earliest_retry_at.min(Some(retry_at));
                            }
                        }
                    }
                }

                let waiting_for = min_block_needed.max(max_block_needed);

                if let Some(max_wait) = max_wait {
                    match ranked_rpcs.should_wait_for_block(waiting_for, skip_rpcs) {
                        ShouldWaitForBlock::NeverReady => break,
                        ShouldWaitForBlock::Ready => {
                            if start.elapsed() > max_wait {
                                break;
                            }
                        }
                        ShouldWaitForBlock::Wait { .. } => select! {
                            _ = watch_ranked_rpcs.changed() => {
                                // no need to borrow_and_update because we do that at the top of the loop
                            },
                            _ = sleep_until(start + max_wait) => break,
                        },
                    }
                }
            } else if let Some(max_wait) = max_wait {
                select! {
                    _ = watch_ranked_rpcs.changed() => {
                        // no need to borrow_and_update because we do that at the top of the loop
                    },
                    _ = sleep_until(start + max_wait) => break,
                }
            } else {
                break;
            }

            // clear for the next loop
            potential_rpcs.clear();
        }

        if let Some(request_metadata) = request_metadata {
            request_metadata.no_servers.fetch_add(1, Ordering::AcqRel);
        }

        if let Some(retry_at) = earliest_retry_at {
            // TODO: log the server that retry_at came from
            warn!(
                ?skip_rpcs,
                retry_in_s=?retry_at.duration_since(Instant::now()).as_secs_f32(),
                "no servers in {} ready!",
                self,
            );

            Ok(OpenRequestResult::RetryAt(retry_at))
        } else {
            warn!(?skip_rpcs, "no servers in {} ready!", self);

            Ok(OpenRequestResult::NotReady)
        }
    }

    /// get all rpc servers that are not rate limited
    /// this prefers synced servers, but it will return servers even if they aren't fully in sync.
    /// This is useful for broadcasting signed transactions.
    // TODO: better type on this that can return an anyhow::Result
    // TODO: this is broken
    pub async fn all_connections(
        &self,
        request_metadata: Option<&Arc<RequestMetadata>>,
        min_block_needed: Option<&U64>,
        max_block_needed: Option<&U64>,
        max_count: Option<usize>,
        error_level: Option<RequestErrorHandler>,
    ) -> Result<Vec<OpenRequestHandle>, Option<Instant>> {
        let mut earliest_retry_at = None;

        // TODO: filter the rpcs with Ranked.will_work_now
        let mut all_rpcs: Vec<_> = self.by_name.read().values().cloned().collect();

        let mut max_count = if let Some(max_count) = max_count {
            max_count
        } else {
            all_rpcs.len()
        };

        trace!("max_count: {}", max_count);

        if max_count == 0 {
            // TODO: return a future that resolves when we know a head block?
            return Err(None);
        }

        let mut selected_rpcs = Vec::with_capacity(max_count);

        // TODO: this sorts them all even though we probably won't need all of them. think about this more
        all_rpcs.sort_by_cached_key(|x| x.sort_for_load_balancing_on(max_block_needed.copied()));

        trace!("all_rpcs: {:#?}", all_rpcs);

        let authorization = request_metadata
            .and_then(|x| x.authorization.clone())
            .unwrap_or_default();

        for rpc in all_rpcs {
            trace!("trying {}", rpc);

            // TODO: use a helper function for these
            if let Some(block_needed) = min_block_needed {
                if !rpc.has_block_data(block_needed) {
                    trace!("{} is missing min_block_needed. skipping", rpc);
                    continue;
                }
            }

            if let Some(block_needed) = max_block_needed {
                if !rpc.has_block_data(block_needed) {
                    trace!("{} is missing max_block_needed. skipping", rpc);
                    continue;
                }
            }

            // check rate limits and increment our connection counter
            match rpc.try_request_handle(&authorization, error_level).await {
                Ok(OpenRequestResult::RetryAt(retry_at)) => {
                    // this rpc is not available. skip it
                    trace!("{} is rate limited. skipping", rpc);
                    earliest_retry_at = earliest_retry_at.min(Some(retry_at));
                }
                Ok(OpenRequestResult::Handle(handle)) => {
                    trace!("{} is available", rpc);
                    selected_rpcs.push(handle);

                    max_count -= 1;
                    if max_count == 0 {
                        break;
                    }
                }
                Ok(OpenRequestResult::NotReady) => {
                    warn!("no request handle for {}", rpc)
                }
                Err(err) => {
                    warn!(?err, "error getting request handle for {}", rpc)
                }
            }
        }

        if !selected_rpcs.is_empty() {
            return Ok(selected_rpcs);
        }

        // return the earliest retry_after (if no rpcs are synced, this will be None)
        Err(earliest_retry_at)
    }

    pub async fn internal_request<P: JsonRpcParams, R: JsonRpcResultData>(
        &self,
        method: &str,
        params: &P,
        max_tries: Option<usize>,
        max_wait: Option<Duration>,
    ) -> Web3ProxyResult<R> {
        // TODO: no request_metadata means we won't have stats on this internal request.
        self.request_with_metadata_and_retries(
            method, params, None, max_tries, max_wait, None, None,
        )
        .await
    }

    /// Make a request with stat tracking.
    #[allow(clippy::too_many_arguments)]
    pub async fn request_with_metadata_and_retries<P: JsonRpcParams, R: JsonRpcResultData>(
        &self,
        method: &str,
        params: &P,
        request_metadata: Option<&Arc<RequestMetadata>>,
        max_tries: Option<usize>,
        max_wait: Option<Duration>,
        min_block_needed: Option<&U64>,
        max_block_needed: Option<&U64>,
    ) -> Web3ProxyResult<R> {
        let mut tries = max_tries.unwrap_or(1);

        let mut last_error = None;

        while tries > 0 {
            tries -= 1;

            match self
                .request_with_metadata(
                    method,
                    params,
                    request_metadata,
                    max_wait,
                    min_block_needed,
                    max_block_needed,
                )
                .await
            {
                Ok(x) => return Ok(x),
                Err(Web3ProxyError::JsonRpcErrorData(err)) => {
                    // TODO: retry some of these? i think request_with_metadata is already smart enough though
                    return Err(err.into());
                }
                Err(err) => {
                    // TODO: only log params in dev
                    warn!(rpc=%self, %method, ?params, ?err, %tries, "retry-able error");
                    last_error = Some(err)
                }
            }
        }

        if let Some(err) = last_error {
            return Err(err);
        }

        Err(anyhow::anyhow!("no response, but no error either. this is a bug").into())
    }

    /// Make a request with stat tracking.
    #[allow(clippy::too_many_arguments)]
    pub async fn request_with_metadata<P: JsonRpcParams, R: JsonRpcResultData>(
        &self,
        method: &str,
        params: &P,
        request_metadata: Option<&Arc<RequestMetadata>>,
        max_wait: Option<Duration>,
        min_block_needed: Option<&U64>,
        max_block_needed: Option<&U64>,
    ) -> Web3ProxyResult<R> {
        let mut skip_rpcs = vec![];
        let mut method_not_available_response = None;

        let mut watch_consensus_rpcs = self.watch_ranked_rpcs.subscribe();

        let start = Instant::now();

        // set error_handler to Save. this might be overridden depending on the request_metadata.authorization
        let error_handler = Some(RequestErrorHandler::Save);

        let mut last_provider_error = None;

        // TODO: the loop here feels somewhat redundant with the loop in best_available_rpc
        loop {
            if let Some(max_wait) = max_wait {
                if start.elapsed() > max_wait {
                    trace!("max_wait exceeded");
                    break;
                }
            }

            match self
                .wait_for_best_rpc(
                    request_metadata,
                    &mut skip_rpcs,
                    min_block_needed,
                    max_block_needed,
                    max_wait,
                    error_handler,
                )
                .await?
            {
                OpenRequestResult::Handle(active_request_handle) => {
                    // save the rpc in case we get an error and want to retry on another server
                    // TODO: look at backend_requests instead
                    let rpc = active_request_handle.clone_connection();

                    if let Some(request_metadata) = request_metadata {
                        request_metadata.backend_requests.lock().push(rpc.clone());
                    }

                    let is_backup_response = rpc.backup;

                    match active_request_handle.request::<P, R>(method, params).await {
                        Ok(response) => {
                            // TODO: if there are multiple responses being aggregated, this will only use the last server's backup type
                            if let Some(request_metadata) = request_metadata {
                                request_metadata
                                    .response_from_backup_rpc
                                    .store(is_backup_response, Ordering::Release);

                                request_metadata
                                    .user_error_response
                                    .store(false, Ordering::Release);

                                request_metadata
                                    .error_response
                                    .store(false, Ordering::Release);
                            }

                            return Ok(response);
                        }
                        Err(error) => {
                            // TODO: if this is an error, do NOT return. continue to try on another server
                            let error = match JsonRpcErrorData::try_from(&error) {
                                Ok(x) => {
                                    if let Some(request_metadata) = request_metadata {
                                        request_metadata
                                            .user_error_response
                                            .store(true, Ordering::Release);
                                    }
                                    x
                                }
                                Err(err) => {
                                    warn!(?err, "error from {}", rpc);

                                    if let Some(request_metadata) = request_metadata {
                                        request_metadata
                                            .error_response
                                            .store(true, Ordering::Release);

                                        request_metadata
                                            .user_error_response
                                            .store(false, Ordering::Release);
                                    }

                                    last_provider_error = Some(error);

                                    continue;
                                }
                            };

                            // some errors should be retried on other nodes
                            let error_msg = error.message.as_ref();

                            // different providers do different codes. check all of them
                            // TODO: there's probably more strings to add here
                            let rate_limit_substrings = ["limit", "exceeded", "quota usage"];
                            for rate_limit_substr in rate_limit_substrings {
                                if error_msg.contains(rate_limit_substr) {
                                    if error_msg.contains("block size") {
                                        // TODO: this message is likely wrong, but i can't find the actual one in my terminal now
                                        // they hit an expected limit. return the error now
                                        return Err(error.into());
                                    } else if error_msg.contains("result on length") {
                                        // this error contains "limit" but is not a rate limit error
                                        // TODO: make the expected limit configurable
                                        // TODO: parse the rate_limit_substr and only continue if it is < expected limit
                                        if error_msg.contains("exceeding limit 2000000") {
                                            // they hit our expected limit. return the error now
                                            return Err(error.into());
                                        } else {
                                            // they hit a limit lower than what we expect. the server is misconfigured
                                            error!(
                                                %error_msg,
                                                "unexpected result limit by {}",
                                                skip_rpcs.last().unwrap(),
                                            );
                                            continue;
                                        }
                                    } else {
                                        warn!(
                                            %error_msg,
                                            "rate limited by {}",
                                            skip_rpcs.last().unwrap()
                                        );
                                        continue;
                                    }
                                }
                            }

                            match error.code {
                                -32000 => {
                                    // TODO: regex?
                                    let retry_prefixes = [
                                        "header not found",
                                        "header for hash not found",
                                        "missing trie node",
                                        "node not started",
                                        "RPC timeout",
                                    ];
                                    for retry_prefix in retry_prefixes {
                                        if error_msg.starts_with(retry_prefix) {
                                            // TODO: too verbose
                                            debug!("retrying on another server");
                                            continue;
                                        }
                                    }
                                }
                                -32601 => {
                                    let error_msg = error.message.as_ref();

                                    // sometimes a provider does not support all rpc methods
                                    // we check other connections rather than returning the error
                                    // but sometimes the method is something that is actually unsupported,
                                    // so we save the response here to return it later

                                    // some providers look like this
                                    if error_msg.starts_with("the method")
                                        && error_msg.ends_with("is not available")
                                    {
                                        method_not_available_response = Some(error);
                                        continue;
                                    }

                                    // others look like this (this is the example in the official spec)
                                    if error_msg == "Method not found" {
                                        method_not_available_response = Some(error);
                                        continue;
                                    }
                                }
                                _ => {}
                            }

                            // let rpc = skip_rpcs
                            //     .last()
                            //     .expect("there must have been a provider if we got an error");

                            // TODO: emit a stat. if a server is getting skipped a lot, something is not right

                            // TODO: if we get a TrySendError, reconnect. wait why do we see a trysenderror on a dual provider? shouldn't it be using reqwest

                            // TODO! WRONG! ONLY SET RETRY_AT IF THIS IS A SERVER/CONNECTION ERROR. JSONRPC "error" is FINE
                            // trace!(
                            //     "Backend server error on {}! Retrying {:?} on another. err={:?}",
                            //     rpc,
                            //     request,
                            //     error,
                            // );
                            // if let Some(ref hard_limit_until) = rpc.hard_limit_until {
                            //     let retry_at = Instant::now() + Duration::from_secs(1);

                            //     hard_limit_until.send_replace(retry_at);
                            // }

                            return Err(error.into());
                        }
                    }
                }
                OpenRequestResult::RetryAt(retry_at) => {
                    // TODO: move this to a helper function
                    // sleep (TODO: with a lock?) until our rate limits should be available
                    // TODO: if a server catches up sync while we are waiting, we could stop waiting
                    warn!(
                        "All rate limits exceeded. waiting for change in synced servers or {:?}s",
                        retry_at.duration_since(Instant::now()).as_secs_f32()
                    );

                    // TODO: have a separate column for rate limited?
                    if let Some(request_metadata) = request_metadata {
                        request_metadata.no_servers.fetch_add(1, Ordering::AcqRel);
                    }

                    tokio::select! {
                        _ = sleep_until(retry_at) => {
                            trace!("slept!");
                            skip_rpcs.pop();
                        }
                        _ = watch_consensus_rpcs.changed() => {
                            watch_consensus_rpcs.borrow_and_update();
                        }
                    }
                }
                OpenRequestResult::NotReady => {
                    if let Some(request_metadata) = request_metadata {
                        request_metadata
                            .error_response
                            .store(true, Ordering::Release);
                    }
                    break;
                }
            }
        }

        if let Some(err) = method_not_available_response {
            if let Some(request_metadata) = request_metadata {
                request_metadata
                    .error_response
                    .store(false, Ordering::Release);

                request_metadata
                    .user_error_response
                    .store(true, Ordering::Release);
            }

            // this error response is likely the user's fault
            // TODO: emit a stat for unsupported methods. then we can know what there is demand for or if we are missing a feature
            return Err(err.into());
        }

        if let Some(err) = last_provider_error {
            return Err(err.into());
        }

        let num_conns = self.len();
        let num_skipped = skip_rpcs.len();

        let needed = min_block_needed.max(max_block_needed);

        let head_block_num = watch_consensus_rpcs
            .borrow()
            .as_ref()
            .map(|x| *x.head_block.number());

        // TODO: error? warn? debug? trace?
        if head_block_num.is_none() {
            error!(
                min=?min_block_needed,
                max=?max_block_needed,
                head=?head_block_num,
                known=num_conns,
                %method,
                ?params,
                "No servers synced",
            );
        } else if head_block_num.as_ref() > needed {
            // we have synced past the needed block
            // TODO: log ranked rpcs
            // TODO: only log params in development
            error!(
                min=?min_block_needed,
                max=?max_block_needed,
                head=?head_block_num,
                known=%num_conns,
                %method,
                ?params,
                "No archive servers synced",
            );
        } else {
            // TODO: only log params in development
            // TODO: log ranked rpcs
            error!(
                min=?min_block_needed,
                max=?max_block_needed,
                head=?head_block_num,
                skipped=%num_skipped,
                known=%num_conns,
                %method,
                ?params,
                "Requested data is not available",
            );
        }

        // TODO: what error code?
        // cloudflare gives {"jsonrpc":"2.0","error":{"code":-32043,"message":"Requested data cannot be older than 128 blocks."},"id":1}
        Err(JsonRpcErrorData {
            message: "Requested data is not available".into(),
            code: -32043,
            data: None,
        }
        .into())
    }

    /// be sure there is a timeout on this or it might loop forever
    #[allow(clippy::too_many_arguments)]
    pub async fn try_send_all_synced_connections<P: JsonRpcParams>(
        self: &Arc<Self>,
        method: &str,
        params: &P,
        request_metadata: Option<&Arc<RequestMetadata>>,
        min_block_needed: Option<&U64>,
        max_block_needed: Option<&U64>,
        max_wait: Option<Duration>,
        error_level: Option<RequestErrorHandler>,
        max_sends: Option<usize>,
    ) -> Web3ProxyResult<Box<RawValue>> {
        let mut watch_consensus_rpcs = self.watch_ranked_rpcs.subscribe();

        let start = Instant::now();

        loop {
            if let Some(max_wait) = max_wait {
                if start.elapsed() > max_wait {
                    break;
                }
            }

            match self
                .all_connections(
                    request_metadata,
                    min_block_needed,
                    max_block_needed,
                    max_sends,
                    error_level,
                )
                .await
            {
                Ok(active_request_handles) => {
                    if let Some(request_metadata) = request_metadata {
                        let mut only_backups_used = true;

                        request_metadata.backend_requests.lock().extend(
                            active_request_handles.iter().map(|x| {
                                let rpc = x.clone_connection();

                                if !rpc.backup {
                                    // TODO: its possible we serve from a synced connection though. think about this more
                                    only_backups_used = false;
                                }

                                rpc
                            }),
                        );

                        request_metadata
                            .response_from_backup_rpc
                            .store(only_backups_used, Ordering::Release);
                    }

                    let x = self
                        .try_send_parallel_requests(active_request_handles, method, params)
                        .await?;

                    return Ok(x);
                }
                Err(None) => {
                    warn!(
                        ?self,
                        ?min_block_needed,
                        ?max_block_needed,
                        "No servers in sync on! Retrying",
                    );

                    if let Some(request_metadata) = &request_metadata {
                        // TODO: if this times out, i think we drop this
                        request_metadata.no_servers.fetch_add(1, Ordering::AcqRel);
                    }

                    let max_sleep = if let Some(max_wait) = max_wait {
                        start + max_wait
                    } else {
                        break;
                    };

                    tokio::select! {
                        _ = sleep_until(max_sleep) => {
                            // rpcs didn't change and we have waited too long. break to return an error
                            warn!(?self, "timeout waiting for try_send_all_synced_connections!");
                            break;
                        },
                        _ = watch_consensus_rpcs.changed() => {
                            // consensus rpcs changed!
                            watch_consensus_rpcs.borrow_and_update();
                            // continue to try again
                            continue;
                        }
                    }
                }
                Err(Some(retry_at)) => {
                    if let Some(request_metadata) = &request_metadata {
                        request_metadata.no_servers.fetch_add(1, Ordering::AcqRel);
                    }

                    if let Some(max_wait) = max_wait {
                        if start.elapsed() > max_wait {
                            warn!(
                                ?self,
                                "All rate limits exceeded. And sleeping would take too long"
                            );
                            break;
                        }

                        warn!("All rate limits exceeded. Sleeping");

                        // TODO: only make one of these sleep_untils

                        tokio::select! {
                            _ = sleep_until(start + max_wait) => {break}
                            _ = sleep_until(retry_at) => {}
                            _ = watch_consensus_rpcs.changed() => {
                                watch_consensus_rpcs.borrow_and_update();
                            }
                        }

                        continue;
                    } else {
                        warn!(?self, "all rate limits exceeded");
                        break;
                    }
                }
            }
        }

        Err(Web3ProxyError::NoServersSynced)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn try_proxy_connection<P: JsonRpcParams, R: JsonRpcResultData>(
        &self,
        method: &str,
        params: &P,
        request_metadata: Option<&Arc<RequestMetadata>>,
        max_tries: Option<usize>,
        max_wait: Option<Duration>,
        min_block_needed: Option<&U64>,
        max_block_needed: Option<&U64>,
    ) -> Web3ProxyResult<R> {
        let proxy_mode = request_metadata.map(|x| x.proxy_mode()).unwrap_or_default();

        match proxy_mode {
            ProxyMode::Debug | ProxyMode::Best => {
                self.request_with_metadata_and_retries(
                    method,
                    params,
                    request_metadata,
                    max_tries,
                    max_wait,
                    min_block_needed,
                    max_block_needed,
                )
                .await
            }
            ProxyMode::Fastest(_x) => todo!("Fastest"),
            ProxyMode::Versus => todo!("Versus"),
        }
    }
}

impl Display for Web3Rpcs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}

impl fmt::Debug for Web3Rpcs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        let consensus_rpcs = self.watch_ranked_rpcs.borrow().is_some();

        f.debug_struct("Web3Rpcs")
            .field("rpcs", &self.by_name)
            .field("consensus_rpcs", &consensus_rpcs)
            .finish_non_exhaustive()
    }
}

impl Serialize for Web3Rpcs {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("Web3Rpcs", 6)?;

        {
            let by_name = self.by_name.read();
            let rpcs: Vec<&Arc<Web3Rpc>> = by_name.values().collect();
            // TODO: coordinate with frontend team to rename "conns" to "rpcs"
            state.serialize_field("conns", &rpcs)?;
        }

        {
            let consensus_rpcs = self.watch_ranked_rpcs.borrow().clone();
            // TODO: rename synced_connections to consensus_rpcs

            if let Some(consensus_rpcs) = consensus_rpcs.as_ref() {
                state.serialize_field("synced_connections", consensus_rpcs)?;
            } else {
                state.serialize_field("synced_connections", &None::<()>)?;
            }
        }

        state.serialize_field(
            "caches",
            &(
                MokaCacheSerializer(&self.blocks_by_hash),
                MokaCacheSerializer(&self.blocks_by_number),
                MokaCacheSerializer(&self.pending_transaction_cache),
            ),
        )?;

        state.serialize_field("block_sender_len", &self.block_sender.len())?;

        state.serialize_field(
            "watch_consensus_rpcs_receivers",
            &self.watch_ranked_rpcs.receiver_count(),
        )?;

        if let Some(ref x) = self.watch_head_block {
            state.serialize_field("watch_consensus_head_receivers", &x.receiver_count())?;
        } else {
            state.serialize_field("watch_consensus_head_receivers", &None::<()>)?;
        }

        state.end()
    }
}

mod tests {
    #![allow(unused_imports)]

    use super::*;
    use crate::rpcs::blockchain::Web3ProxyBlock;
    use crate::rpcs::consensus::ConsensusFinder;
    use arc_swap::ArcSwap;
    use ethers::types::H256;
    use ethers::types::{Block, U256};
    use latency::PeakEwmaLatency;
    use moka::future::CacheBuilder;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tracing::trace;

    #[cfg(test)]
    fn new_peak_latency() -> PeakEwmaLatency {
        PeakEwmaLatency::spawn(Duration::from_secs(1), 4, Duration::from_secs(1))
    }

    #[test_log::test(tokio::test)]
    async fn test_sort_connections_by_sync_status() {
        let block_0 = Block {
            number: Some(0.into()),
            hash: Some(H256::random()),
            ..Default::default()
        };
        let block_1 = Block {
            number: Some(1.into()),
            hash: Some(H256::random()),
            parent_hash: block_0.hash.unwrap(),
            ..Default::default()
        };
        let block_2 = Block {
            number: Some(2.into()),
            hash: Some(H256::random()),
            parent_hash: block_1.hash.unwrap(),
            ..Default::default()
        };

        let blocks: Vec<_> = [block_0, block_1, block_2]
            .into_iter()
            .map(|x| Web3ProxyBlock::try_new(Arc::new(x)).unwrap())
            .collect();

        let (tx_a, _) = watch::channel(None);
        let (tx_b, _) = watch::channel(blocks.get(1).cloned());
        let (tx_c, _) = watch::channel(blocks.get(2).cloned());
        let (tx_d, _) = watch::channel(None);
        let (tx_e, _) = watch::channel(blocks.get(1).cloned());
        let (tx_f, _) = watch::channel(blocks.get(2).cloned());

        let mut rpcs: Vec<_> = [
            Web3Rpc {
                name: "a".to_string(),
                tier: 0.into(),
                head_block: Some(tx_a),
                peak_latency: Some(new_peak_latency()),
                ..Default::default()
            },
            Web3Rpc {
                name: "b".to_string(),
                tier: 0.into(),
                head_block: Some(tx_b),
                peak_latency: Some(new_peak_latency()),
                ..Default::default()
            },
            Web3Rpc {
                name: "c".to_string(),
                tier: 0.into(),
                head_block: Some(tx_c),
                peak_latency: Some(new_peak_latency()),
                ..Default::default()
            },
            Web3Rpc {
                name: "d".to_string(),
                tier: 1.into(),
                head_block: Some(tx_d),
                peak_latency: Some(new_peak_latency()),
                ..Default::default()
            },
            Web3Rpc {
                name: "e".to_string(),
                tier: 1.into(),
                head_block: Some(tx_e),
                peak_latency: Some(new_peak_latency()),
                ..Default::default()
            },
            Web3Rpc {
                name: "f".to_string(),
                tier: 1.into(),
                head_block: Some(tx_f),
                peak_latency: Some(new_peak_latency()),
                ..Default::default()
            },
        ]
        .into_iter()
        .map(Arc::new)
        .collect();

        rpcs.sort_by_cached_key(|x| x.sort_for_load_balancing_on(None));

        let names_in_sort_order: Vec<_> = rpcs.iter().map(|x| x.name.as_str()).collect();

        // assert_eq!(names_in_sort_order, ["c", "b", "a", "f", "e", "d"]);
        assert_eq!(names_in_sort_order, ["c", "f", "b", "e", "a", "d"]);
    }

    #[test_log::test(tokio::test)]
    async fn test_server_selection_by_height() {
        let now = chrono::Utc::now().timestamp().into();

        let lagged_block = Block {
            hash: Some(H256::random()),
            number: Some(0.into()),
            timestamp: now - 1,
            ..Default::default()
        };

        let head_block = Block {
            hash: Some(H256::random()),
            number: Some(1.into()),
            parent_hash: lagged_block.hash.unwrap(),
            timestamp: now,
            ..Default::default()
        };

        let lagged_block = Arc::new(lagged_block);
        let head_block = Arc::new(head_block);

        let block_data_limit = u64::MAX;

        let (tx_synced, _) = watch::channel(None);

        let head_rpc = Web3Rpc {
            name: "synced".to_string(),
            soft_limit: 1_000,
            automatic_block_limit: false,
            backup: false,
            block_data_limit: block_data_limit.into(),
            head_block: Some(tx_synced),
            peak_latency: Some(new_peak_latency()),
            ..Default::default()
        };

        let (tx_lagged, _) = watch::channel(None);

        let lagged_rpc = Web3Rpc {
            name: "lagged".to_string(),
            soft_limit: 1_000,
            automatic_block_limit: false,
            backup: false,
            block_data_limit: block_data_limit.into(),
            head_block: Some(tx_lagged),
            peak_latency: Some(new_peak_latency()),
            ..Default::default()
        };

        assert!(!head_rpc.has_block_data(lagged_block.number.as_ref().unwrap()));
        assert!(!head_rpc.has_block_data(head_block.number.as_ref().unwrap()));

        assert!(!lagged_rpc.has_block_data(lagged_block.number.as_ref().unwrap()));
        assert!(!lagged_rpc.has_block_data(head_block.number.as_ref().unwrap()));

        let head_rpc = Arc::new(head_rpc);
        let lagged_rpc = Arc::new(lagged_rpc);

        let (block_sender, _block_receiver) = flume::unbounded();
        let (pending_tx_id_sender, pending_tx_id_receiver) = flume::unbounded();
        let (watch_ranked_rpcs, _watch_consensus_rpcs_receiver) = watch::channel(None);
        let (watch_consensus_head_sender, _watch_consensus_head_receiver) = watch::channel(None);

        let chain_id = 1;

        let mut by_name = HashMap::new();
        by_name.insert(head_rpc.name.clone(), head_rpc.clone());
        by_name.insert(lagged_rpc.name.clone(), lagged_rpc.clone());

        // TODO: make a Web3Rpcs::new
        let rpcs = Web3Rpcs {
            block_sender: block_sender.clone(),
            by_name: RwLock::new(by_name),
            chain_id,
            name: "test".to_string(),
            watch_head_block: Some(watch_consensus_head_sender),
            watch_ranked_rpcs,
            pending_transaction_cache: CacheBuilder::new(100)
                .time_to_live(Duration::from_secs(60))
                .build(),
            pending_tx_id_receiver,
            pending_tx_id_sender,
            blocks_by_hash: CacheBuilder::new(100)
                .time_to_live(Duration::from_secs(60))
                .build(),
            blocks_by_number: CacheBuilder::new(100)
                .time_to_live(Duration::from_secs(60))
                .build(),
            // TODO: test max_head_block_age?
            max_head_block_age: Duration::from_secs(60),
            // TODO: test max_head_block_lag?
            max_head_block_lag: 5.into(),
            min_synced_rpcs: 1,
            min_sum_soft_limit: 1,
        };

        let authorization = Arc::new(Authorization::internal(None).unwrap());

        let mut consensus_finder = ConsensusFinder::new(None, None);

        consensus_finder
            .process_block_from_rpc(&rpcs, &authorization, None, lagged_rpc.clone(), &None)
            .await
            .expect(
                "its lagged, but it should still be seen as consensus if its the first to report",
            );
        consensus_finder
            .process_block_from_rpc(&rpcs, &authorization, None, head_rpc.clone(), &None)
            .await
            .unwrap();

        // no head block because the rpcs haven't communicated through their channels
        assert!(rpcs.head_block_hash().is_none());

        // all_backend_connections gives all non-backup servers regardless of sync status
        assert_eq!(
            rpcs.all_connections(None, None, None, None, None)
                .await
                .unwrap()
                .len(),
            2
        );

        // best_synced_backend_connection which servers to be synced with the head block should not find any nodes
        let x = rpcs
            .wait_for_best_rpc(
                None,
                &mut vec![],
                Some(head_block.number.as_ref().unwrap()),
                None,
                Some(Duration::from_secs(0)),
                Some(RequestErrorHandler::DebugLevel),
            )
            .await
            .unwrap();

        info!(?x);

        assert!(matches!(x, OpenRequestResult::NotReady));

        // add lagged blocks to the rpcs. both servers should be allowed
        lagged_rpc
            .send_head_block_result(
                Ok(Some(lagged_block.clone())),
                &block_sender,
                &rpcs.blocks_by_hash,
            )
            .await
            .unwrap();

        // TODO: calling process_block_from_rpc and send_head_block_result seperate seems very fragile
        consensus_finder
            .process_block_from_rpc(
                &rpcs,
                &authorization,
                Some(lagged_block.clone().try_into().unwrap()),
                lagged_rpc.clone(),
                &None,
            )
            .await
            .unwrap();

        head_rpc
            .send_head_block_result(
                Ok(Some(lagged_block.clone())),
                &block_sender,
                &rpcs.blocks_by_hash,
            )
            .await
            .unwrap();

        // TODO: this is fragile
        consensus_finder
            .process_block_from_rpc(
                &rpcs,
                &authorization,
                Some(lagged_block.clone().try_into().unwrap()),
                head_rpc.clone(),
                &None,
            )
            .await
            .unwrap();

        // TODO: how do we spawn this and wait for it to process things? subscribe and watch consensus connections?
        // rpcs.process_incoming_blocks(&authorization, block_receiver, pending_tx_sender)

        assert!(head_rpc.has_block_data(lagged_block.number.as_ref().unwrap()));
        assert!(!head_rpc.has_block_data(head_block.number.as_ref().unwrap()));

        assert!(lagged_rpc.has_block_data(lagged_block.number.as_ref().unwrap()));
        assert!(!lagged_rpc.has_block_data(head_block.number.as_ref().unwrap()));

        assert_eq!(rpcs.num_synced_rpcs(), 2);

        // add head block to the rpcs. lagged_rpc should not be available
        head_rpc
            .send_head_block_result(
                Ok(Some(head_block.clone())),
                &block_sender,
                &rpcs.blocks_by_hash,
            )
            .await
            .unwrap();

        // TODO: this is fragile
        consensus_finder
            .process_block_from_rpc(
                &rpcs,
                &authorization,
                Some(head_block.clone().try_into().unwrap()),
                head_rpc.clone(),
                &None,
            )
            .await
            .unwrap();

        assert_eq!(rpcs.num_synced_rpcs(), 1);

        assert!(head_rpc.has_block_data(lagged_block.number.as_ref().unwrap()));
        assert!(head_rpc.has_block_data(head_block.number.as_ref().unwrap()));

        assert!(lagged_rpc.has_block_data(lagged_block.number.as_ref().unwrap()));
        assert!(!lagged_rpc.has_block_data(head_block.number.as_ref().unwrap()));

        // TODO: make sure the handle is for the expected rpc
        assert!(matches!(
            rpcs.wait_for_best_rpc(
                None,
                &mut vec![],
                None,
                None,
                Some(Duration::from_secs(0)),
                None,
            )
            .await,
            Ok(OpenRequestResult::Handle(_))
        ));

        // TODO: make sure the handle is for the expected rpc
        assert!(matches!(
            rpcs.wait_for_best_rpc(
                None,
                &mut vec![],
                Some(&0.into()),
                None,
                Some(Duration::from_secs(0)),
                None,
            )
            .await,
            Ok(OpenRequestResult::Handle(_))
        ));

        // TODO: make sure the handle is for the expected rpc
        assert!(matches!(
            rpcs.wait_for_best_rpc(
                None,
                &mut vec![],
                Some(&1.into()),
                None,
                Some(Duration::from_secs(0)),
                None,
            )
            .await,
            Ok(OpenRequestResult::Handle(_))
        ));

        // future block should not get a handle
        let future_rpc = rpcs
            .wait_for_best_rpc(
                None,
                &mut vec![],
                Some(&2.into()),
                None,
                Some(Duration::from_secs(0)),
                None,
            )
            .await;
        assert!(matches!(future_rpc, Ok(OpenRequestResult::NotReady)));
    }

    #[test_log::test(tokio::test)]
    async fn test_server_selection_by_archive() {
        let now = chrono::Utc::now().timestamp().into();

        let head_block = Block {
            hash: Some(H256::random()),
            number: Some(1_000_000.into()),
            parent_hash: H256::random(),
            timestamp: now,
            ..Default::default()
        };

        let head_block: Web3ProxyBlock = Arc::new(head_block).try_into().unwrap();

        let (tx_pruned, _) = watch::channel(Some(head_block.clone()));

        let pruned_rpc = Web3Rpc {
            name: "pruned".to_string(),
            soft_limit: 3_000,
            automatic_block_limit: false,
            backup: false,
            block_data_limit: 64.into(),
            tier: 1.into(),
            head_block: Some(tx_pruned),
            ..Default::default()
        };

        let (tx_archive, _) = watch::channel(Some(head_block.clone()));

        let archive_rpc = Web3Rpc {
            name: "archive".to_string(),
            soft_limit: 1_000,
            automatic_block_limit: false,
            backup: false,
            block_data_limit: u64::MAX.into(),
            tier: 2.into(),
            head_block: Some(tx_archive),
            ..Default::default()
        };

        assert!(pruned_rpc.has_block_data(head_block.number()));
        assert!(archive_rpc.has_block_data(head_block.number()));
        assert!(!pruned_rpc.has_block_data(&1.into()));
        assert!(archive_rpc.has_block_data(&1.into()));

        let pruned_rpc = Arc::new(pruned_rpc);
        let archive_rpc = Arc::new(archive_rpc);

        let (block_sender, _) = flume::unbounded();
        let (pending_tx_id_sender, pending_tx_id_receiver) = flume::unbounded();
        let (watch_ranked_rpcs, _) = watch::channel(None);
        let (watch_consensus_head_sender, _watch_consensus_head_receiver) = watch::channel(None);

        let chain_id = 1;

        let mut by_name = HashMap::new();
        by_name.insert(pruned_rpc.name.clone(), pruned_rpc.clone());
        by_name.insert(archive_rpc.name.clone(), archive_rpc.clone());

        let rpcs = Web3Rpcs {
            block_sender,
            by_name: RwLock::new(by_name),
            chain_id,
            name: "test".to_string(),
            watch_head_block: Some(watch_consensus_head_sender),
            watch_ranked_rpcs,
            pending_transaction_cache: CacheBuilder::new(100)
                .time_to_live(Duration::from_secs(120))
                .build(),
            pending_tx_id_receiver,
            pending_tx_id_sender,
            blocks_by_hash: CacheBuilder::new(100)
                .time_to_live(Duration::from_secs(120))
                .build(),
            blocks_by_number: CacheBuilder::new(100)
                .time_to_live(Duration::from_secs(120))
                .build(),
            min_synced_rpcs: 1,
            min_sum_soft_limit: 4_000,
            max_head_block_age: Duration::from_secs(60),
            max_head_block_lag: 5.into(),
        };

        let authorization = Arc::new(Authorization::internal(None).unwrap());

        let mut connection_heads = ConsensusFinder::new(None, None);

        // min sum soft limit will require 2 servers
        let x = connection_heads
            .process_block_from_rpc(
                &rpcs,
                &authorization,
                Some(head_block.clone()),
                pruned_rpc.clone(),
                &None,
            )
            .await
            .unwrap();
        assert!(!x);

        assert_eq!(rpcs.num_synced_rpcs(), 0);

        let x = connection_heads
            .process_block_from_rpc(
                &rpcs,
                &authorization,
                Some(head_block.clone()),
                archive_rpc.clone(),
                &None,
            )
            .await
            .unwrap();
        assert!(x);

        assert_eq!(rpcs.num_synced_rpcs(), 2);

        // best_synced_backend_connection requires servers to be synced with the head block
        // TODO: test with and without passing the head_block.number?
        let best_available_server = rpcs
            .wait_for_best_rpc(
                None,
                &mut vec![],
                Some(head_block.number()),
                None,
                Some(Duration::from_secs(0)),
                None,
            )
            .await;

        debug!("best_available_server: {:#?}", best_available_server);

        assert!(matches!(
            best_available_server.unwrap(),
            OpenRequestResult::Handle(_)
        ));

        let _best_available_server_from_none = rpcs
            .wait_for_best_rpc(
                None,
                &mut vec![],
                None,
                None,
                Some(Duration::from_secs(0)),
                None,
            )
            .await;

        // assert_eq!(best_available_server, best_available_server_from_none);

        let best_archive_server = rpcs
            .wait_for_best_rpc(
                None,
                &mut vec![],
                Some(&1.into()),
                None,
                Some(Duration::from_secs(0)),
                None,
            )
            .await;

        match best_archive_server {
            Ok(OpenRequestResult::Handle(x)) => {
                assert_eq!(x.clone_connection().name, "archive".to_string())
            }
            x => {
                error!("unexpected result: {:?}", x);
            }
        }
    }

    #[test_log::test(tokio::test)]
    async fn test_all_connections() {
        // TODO: use chrono, not SystemTime
        let now: U256 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .into();

        let block_1 = Block {
            hash: Some(H256::random()),
            number: Some(1_000_000.into()),
            parent_hash: H256::random(),
            timestamp: now,
            ..Default::default()
        };
        let block_2 = Block {
            hash: Some(H256::random()),
            number: Some(1_000_001.into()),
            parent_hash: block_1.hash.unwrap(),
            timestamp: now + 1,
            ..Default::default()
        };

        let block_1: Web3ProxyBlock = Arc::new(block_1).try_into().unwrap();
        let block_2: Web3ProxyBlock = Arc::new(block_2).try_into().unwrap();

        let (tx_mock_geth, _) = watch::channel(Some(block_1.clone()));
        let (tx_mock_erigon_archive, _) = watch::channel(Some(block_2.clone()));

        let mock_geth = Web3Rpc {
            name: "mock_geth".to_string(),
            soft_limit: 1_000,
            automatic_block_limit: false,
            backup: false,
            block_data_limit: 64.into(),
            // tier: 0,
            head_block: Some(tx_mock_geth),
            peak_latency: Some(new_peak_latency()),
            ..Default::default()
        };

        let mock_erigon_archive = Web3Rpc {
            name: "mock_erigon_archive".to_string(),
            soft_limit: 1_000,
            automatic_block_limit: false,
            backup: false,
            block_data_limit: u64::MAX.into(),
            // tier: 1,
            head_block: Some(tx_mock_erigon_archive),
            peak_latency: Some(new_peak_latency()),
            ..Default::default()
        };

        assert!(mock_geth.has_block_data(block_1.number()));
        assert!(mock_erigon_archive.has_block_data(block_1.number()));
        assert!(!mock_geth.has_block_data(block_2.number()));
        assert!(mock_erigon_archive.has_block_data(block_2.number()));

        let mock_geth = Arc::new(mock_geth);
        let mock_erigon_archive = Arc::new(mock_erigon_archive);

        let (block_sender, _) = flume::unbounded();
        let (pending_tx_id_sender, pending_tx_id_receiver) = flume::unbounded();
        let (watch_ranked_rpcs, _) = watch::channel(None);
        let (watch_consensus_head_sender, _watch_consensus_head_receiver) = watch::channel(None);

        let chain_id = 1;

        let mut by_name = HashMap::new();
        by_name.insert(mock_geth.name.clone(), mock_geth.clone());
        by_name.insert(
            mock_erigon_archive.name.clone(),
            mock_erigon_archive.clone(),
        );

        // TODO: make a Web3Rpcs::new
        let rpcs = Web3Rpcs {
            block_sender,
            by_name: RwLock::new(by_name),
            chain_id,
            name: "test".to_string(),
            watch_head_block: Some(watch_consensus_head_sender),
            watch_ranked_rpcs,
            pending_transaction_cache: Cache::new(10_000),
            pending_tx_id_receiver,
            pending_tx_id_sender,
            blocks_by_hash: Cache::new(10_000),
            blocks_by_number: Cache::new(10_000),
            min_synced_rpcs: 1,
            min_sum_soft_limit: 1_000,
            max_head_block_age: Duration::from_secs(60),
            max_head_block_lag: 5.into(),
        };

        let authorization = Arc::new(Authorization::internal(None).unwrap());

        let mut consensus_finder = ConsensusFinder::new(None, None);

        consensus_finder
            .process_block_from_rpc(
                &rpcs,
                &authorization,
                Some(block_1.clone()),
                mock_geth.clone(),
                &None,
            )
            .await
            .unwrap();

        consensus_finder
            .process_block_from_rpc(
                &rpcs,
                &authorization,
                Some(block_2.clone()),
                mock_erigon_archive.clone(),
                &None,
            )
            .await
            .unwrap();

        assert_eq!(rpcs.num_synced_rpcs(), 1);

        // best_synced_backend_connection requires servers to be synced with the head block
        // TODO: test with and without passing the head_block.number?
        let head_connections = rpcs
            .all_connections(None, Some(block_2.number()), None, None, None)
            .await;

        debug!("head_connections: {:#?}", head_connections);

        assert_eq!(
            head_connections.unwrap().len(),
            1,
            "wrong number of connections"
        );

        let all_connections = rpcs
            .all_connections(None, Some(block_1.number()), None, None, None)
            .await;

        debug!("all_connections: {:#?}", all_connections);

        assert_eq!(
            all_connections.unwrap().len(),
            2,
            "wrong number of connections"
        );

        let all_connections = rpcs.all_connections(None, None, None, None, None).await;

        debug!("all_connections: {:#?}", all_connections);

        assert_eq!(
            all_connections.unwrap().len(),
            2,
            "wrong number of connections"
        )
    }
}

#[cfg(test)]
mod test {
    use std::cmp::Reverse;

    #[test]
    fn test_block_num_sort() {
        let test_vec = vec![
            Reverse(Some(3)),
            Reverse(Some(2)),
            Reverse(Some(1)),
            Reverse(None),
        ];

        let mut sorted_vec = test_vec.clone();
        sorted_vec.sort();

        assert_eq!(test_vec, sorted_vec);
    }
}
