///! Load balanced communication with a group of web3 rpc providers
use super::blockchain::{BlocksByHashCache, BlocksByNumberCache, Web3ProxyBlock};
use super::consensus::{ConsensusWeb3Rpcs, ShouldWaitForBlock};
use super::one::Web3Rpc;
use super::request::{OpenRequestHandle, OpenRequestResult, RequestErrorHandler};
use crate::app::{flatten_handle, Web3ProxyApp, Web3ProxyJoinHandle};
use crate::config::{BlockAndRpc, TxHashAndRpc, Web3RpcConfig};
use crate::frontend::authorization::{Authorization, RequestMetadata};
use crate::frontend::errors::{Web3ProxyError, Web3ProxyResult};
use crate::frontend::rpc_proxy_ws::ProxyMode;
use crate::jsonrpc::{JsonRpcErrorData, JsonRpcRequest};
use crate::response_cache::JsonRpcResponseData;
use crate::rpcs::transactions::TxStatus;
use arc_swap::ArcSwap;
use counter::Counter;
use derive_more::From;
use ethers::prelude::{ProviderError, TxHash, U64};
use futures::future::try_join_all;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use hashbrown::{HashMap, HashSet};
use itertools::Itertools;
use log::{debug, error, info, trace, warn, Level};
use migration::sea_orm::DatabaseConnection;
use ordered_float::OrderedFloat;
use quick_cache_ttl::CacheWithTTL;
use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;
use serde_json::json;
use serde_json::value::RawValue;
use std::borrow::Cow;
use std::cmp::{min_by_key, Reverse};
use std::fmt::{self, Display};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use thread_fast_rng::rand::seq::SliceRandom;
use tokio::select;
use tokio::sync::{broadcast, watch};
use tokio::time::{sleep, sleep_until, Duration, Instant};

/// A collection of web3 connections. Sends requests either the current best server or all servers.
#[derive(From)]
pub struct Web3Rpcs {
    pub(crate) name: String,
    /// if watch_consensus_head_sender is some, Web3Rpc inside self will send blocks here when they get them
    pub(crate) block_sender: flume::Sender<(Option<Web3ProxyBlock>, Arc<Web3Rpc>)>,
    /// any requests will be forwarded to one (or more) of these connections
    pub(crate) by_name: ArcSwap<HashMap<String, Arc<Web3Rpc>>>,
    /// all providers with the same consensus head block. won't update if there is no `self.watch_consensus_head_sender`
    /// TODO: document that this is a watch sender and not a broadcast! if things get busy, blocks might get missed
    /// TODO: why is watch_consensus_head_sender in an Option, but this one isn't?
    /// Geth's subscriptions have the same potential for skipping blocks.
    pub(crate) watch_consensus_rpcs_sender: watch::Sender<Option<Arc<ConsensusWeb3Rpcs>>>,
    /// this head receiver makes it easy to wait until there is a new block
    pub(super) watch_consensus_head_sender: Option<watch::Sender<Option<Web3ProxyBlock>>>,
    /// keep track of transactions that we have sent through subscriptions
    pub(super) pending_transaction_cache: Arc<CacheWithTTL<TxHash, TxStatus>>,
    pub(super) pending_tx_id_receiver: flume::Receiver<TxHashAndRpc>,
    pub(super) pending_tx_id_sender: flume::Sender<TxHashAndRpc>,
    /// TODO: this map is going to grow forever unless we do some sort of pruning. maybe store pruned in redis?
    /// all blocks, including orphans
    pub(super) blocks_by_hash: BlocksByHashCache,
    /// blocks on the heaviest chain
    pub(super) blocks_by_number: BlocksByNumberCache,
    /// the number of rpcs required to agree on consensus for the head block (thundering herd protection)
    pub(super) min_head_rpcs: usize,
    /// the soft limit required to agree on consensus for the head block. (thundering herd protection)
    pub(super) min_sum_soft_limit: u32,
    /// how far behind the highest known block height we can be before we stop serving requests
    pub(super) max_block_lag: Option<U64>,
    /// how old our consensus head block we can be before we stop serving requests
    pub(super) max_block_age: Option<u64>,
}

impl Web3Rpcs {
    /// Spawn durable connections to multiple Web3 providers.
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn(
        db_conn: Option<DatabaseConnection>,
        max_block_age: Option<u64>,
        max_block_lag: Option<U64>,
        min_head_rpcs: usize,
        min_sum_soft_limit: u32,
        name: String,
        pending_transaction_cache: Arc<CacheWithTTL<TxHash, TxStatus>>,
        pending_tx_sender: Option<broadcast::Sender<TxStatus>>,
        watch_consensus_head_sender: Option<watch::Sender<Option<Web3ProxyBlock>>>,
    ) -> anyhow::Result<(
        Arc<Self>,
        Web3ProxyJoinHandle<()>,
        watch::Receiver<Option<Arc<ConsensusWeb3Rpcs>>>,
        // watch::Receiver<Arc<ConsensusWeb3Rpcs>>,
    )> {
        let (pending_tx_id_sender, pending_tx_id_receiver) = flume::unbounded();
        let (block_sender, block_receiver) = flume::unbounded::<BlockAndRpc>();

        // these blocks don't have full transactions, but they do have rather variable amounts of transaction hashes
        // TODO: actual weighter on this
        // TODO: time_to_idle instead?
        let blocks_by_hash: BlocksByHashCache = Arc::new(
            CacheWithTTL::new("blocks_by_hash", 10_000, Duration::from_secs(30 * 60)).await,
        );

        // all block numbers are the same size, so no need for weigher
        // TODO: limits from config
        // TODO: time_to_idle instead?
        let blocks_by_number = Arc::new(
            CacheWithTTL::new("blocks_by_number", 10_000, Duration::from_secs(30 * 60)).await,
        );

        let (watch_consensus_rpcs_sender, consensus_connections_watcher) =
            watch::channel(Default::default());

        // by_name starts empty. self.apply_server_configs will add to it
        let by_name = Default::default();

        let connections = Arc::new(Self {
            block_sender,
            blocks_by_hash,
            blocks_by_number,
            by_name,
            max_block_age,
            max_block_lag,
            min_head_rpcs,
            min_sum_soft_limit,
            name,
            pending_transaction_cache,
            pending_tx_id_receiver,
            pending_tx_id_sender,
            watch_consensus_head_sender,
            watch_consensus_rpcs_sender,
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

        // turn configs into connections (in parallel)
        let mut spawn_handles: FuturesUnordered<_> = rpc_configs
            .into_iter()
            .filter_map(|(server_name, server_config)| {
                if server_config.disabled {
                    info!("{} is disabled", server_name);
                    return None;
                }

                let db_conn = app.db_conn();
                let http_client = app.http_client.clone();
                let vredis_pool = app.vredis_pool.clone();

                let block_sender = if self.watch_consensus_head_sender.is_some() {
                    Some(self.block_sender.clone())
                } else {
                    None
                };

                let pending_tx_id_sender = Some(self.pending_tx_id_sender.clone());
                let blocks_by_hash_cache = self.blocks_by_hash.clone();
                let chain_id = app.config.chain_id;

                debug!("spawning {}", server_name);

                let handle = tokio::spawn(server_config.spawn(
                    server_name,
                    db_conn,
                    vredis_pool,
                    chain_id,
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
                Ok(Ok((rpc, _handle))) => {
                    // web3 connection worked
                    let mut new_by_name = (*self.by_name.load_full()).clone();

                    let old_rpc = new_by_name.insert(rpc.name.clone(), rpc.clone());

                    self.by_name.store(Arc::new(new_by_name));

                    if let Some(old_rpc) = old_rpc {
                        if old_rpc.head_block.as_ref().unwrap().borrow().is_some() {
                            let mut new_head_receiver =
                                rpc.head_block.as_ref().unwrap().subscribe();
                            debug!("waiting for new {} to sync", rpc);

                            // TODO: maximum wait time or this could block things for too long
                            while new_head_receiver.borrow_and_update().is_none() {
                                new_head_receiver.changed().await?;
                            }

                            // TODO: tell ethers to disconnect? is there a function for that?
                        }
                    }

                    // TODO: what should we do with the new handle? make sure error logs aren't dropped
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

        let num_rpcs = self.by_name.load().len();

        if num_rpcs < self.min_head_rpcs {
            return Err(Web3ProxyError::NotEnoughRpcs {
                num_known: num_rpcs,
                min_head_rpcs: self.min_head_rpcs,
            });
        }

        Ok(())
    }

    pub fn get(&self, conn_name: &str) -> Option<Arc<Web3Rpc>> {
        self.by_name.load().get(conn_name).cloned()
    }

    pub fn len(&self) -> usize {
        self.by_name.load().len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.load().is_empty()
    }

    pub fn min_head_rpcs(&self) -> usize {
        self.min_head_rpcs
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
        if self.watch_consensus_head_sender.is_some() {
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
    pub async fn try_send_parallel_requests(
        &self,
        active_request_handles: Vec<OpenRequestHandle>,
        method: &str,
        params: Option<&serde_json::Value>,
        error_level: Level,
        // TODO: remove this box once i figure out how to do the options
    ) -> Web3ProxyResult<JsonRpcResponseData> {
        // TODO: if only 1 active_request_handles, do self.try_send_request?

        let responses = active_request_handles
            .into_iter()
            .map(|active_request_handle| async move {
                let result: Result<Box<RawValue>, _> = active_request_handle
                    .request(method, &json!(&params), error_level.into())
                    .await;
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

        for (most_common, _) in counts.most_common_ordered() {
            let most_common = count_map
                .remove(&most_common)
                .expect("most_common key must exist");

            match most_common {
                Ok(x) => {
                    // return the most common success
                    return Ok(x.into());
                }
                Err(err) => {
                    if any_ok_with_json_result {
                        // the most common is an error, but there is an Ok in here somewhere. loop to find it
                        continue;
                    }

                    let err: JsonRpcErrorData = err.try_into()?;

                    return Ok(err.into());
                }
            }
        }

        // TODO: what should we do if we get here? i don't think we will
        unimplemented!("this shouldn't be possible")
    }

    async fn _best_available_rpc(
        &self,
        authorization: &Arc<Authorization>,
        potential_rpcs: &[Arc<Web3Rpc>],
        skip: &mut Vec<Arc<Web3Rpc>>,
    ) -> OpenRequestResult {
        let mut earliest_retry_at = None;

        for (rpc_a, rpc_b) in potential_rpcs.iter().circular_tuple_windows() {
            trace!("{} vs {}", rpc_a, rpc_b);
            // TODO: cached key to save a read lock
            // TODO: ties to the server with the smallest block_data_limit
            let faster_rpc = min_by_key(rpc_a, rpc_b, |x| x.peak_ewma());
            trace!("winner: {}", faster_rpc);

            // add to the skip list in case this one fails
            skip.push(Arc::clone(faster_rpc));

            // just because it has lower latency doesn't mean we are sure to get a connection. there might be rate limits
            match faster_rpc.try_request_handle(authorization).await {
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
        authorization: &Arc<Authorization>,
        request_metadata: Option<&Arc<RequestMetadata>>,
        skip_rpcs: &mut Vec<Arc<Web3Rpc>>,
        min_block_needed: Option<&U64>,
        max_block_needed: Option<&U64>,
        max_wait: Option<Duration>,
    ) -> Web3ProxyResult<OpenRequestResult> {
        let mut earliest_retry_at: Option<Instant> = None;

        if self.watch_consensus_head_sender.is_none() {
            trace!("this Web3Rpcs is not tracking head blocks. pick any server");

            let by_name = self.by_name.load();

            let mut potential_rpcs: Vec<_> = by_name
                .values()
                .filter(|rpc| !skip_rpcs.contains(rpc))
                .filter(|rpc| {
                    min_block_needed
                        .map(|x| rpc.has_block_data(x))
                        .unwrap_or(true)
                })
                .filter(|rpc| {
                    max_block_needed
                        .map(|x| rpc.has_block_data(x))
                        .unwrap_or(true)
                })
                .cloned()
                .collect();

            // TODO: include tiers in this?
            potential_rpcs.shuffle(&mut thread_fast_rng::thread_fast_rng());

            match self
                ._best_available_rpc(authorization, &potential_rpcs, skip_rpcs)
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
        } else {
            let stop_trying_at =
                Instant::now() + max_wait.unwrap_or_else(|| Duration::from_secs(10));

            let mut watch_consensus_rpcs = self.watch_consensus_rpcs_sender.subscribe();

            let mut potential_rpcs = Vec::with_capacity(self.by_name.load().len());

            loop {
                let consensus_rpcs = watch_consensus_rpcs.borrow_and_update().clone();

                potential_rpcs.clear();

                // first check everything that is synced
                // even though we might be querying an old block that an unsynced server can handle,
                // it is best to not send queries to a syncing server. that slows down sync and can bloat erigon's disk usage.
                if let Some(consensus_rpcs) = consensus_rpcs {
                    potential_rpcs.extend(
                        consensus_rpcs
                            .head_rpcs
                            .iter()
                            .filter(|rpc| {
                                consensus_rpcs.rpc_will_work_now(
                                    skip_rpcs,
                                    min_block_needed,
                                    max_block_needed,
                                    rpc,
                                )
                            })
                            .cloned(),
                    );

                    potential_rpcs.shuffle(&mut thread_fast_rng::thread_fast_rng());

                    if potential_rpcs.len() >= self.min_head_rpcs {
                        // we have enough potential rpcs. try to load balance

                        match self
                            ._best_available_rpc(authorization, &potential_rpcs, skip_rpcs)
                            .await
                        {
                            OpenRequestResult::Handle(x) => {
                                return Ok(OpenRequestResult::Handle(x))
                            }
                            OpenRequestResult::NotReady => {}
                            OpenRequestResult::RetryAt(retry_at) => {
                                if earliest_retry_at.is_none() {
                                    earliest_retry_at = Some(retry_at);
                                } else {
                                    earliest_retry_at = earliest_retry_at.min(Some(retry_at));
                                }
                            }
                        }

                        // these rpcs were tried. don't try them again
                        potential_rpcs.clear();
                    }

                    for next_rpcs in consensus_rpcs.other_rpcs.values() {
                        // we have to collect in order to shuffle
                        let mut more_rpcs: Vec<_> = next_rpcs
                            .iter()
                            .filter(|rpc| {
                                consensus_rpcs.rpc_will_work_now(
                                    skip_rpcs,
                                    min_block_needed,
                                    max_block_needed,
                                    rpc,
                                )
                            })
                            .cloned()
                            .collect();

                        // shuffle only the new entries. that way the highest tier still gets preference
                        more_rpcs.shuffle(&mut thread_fast_rng::thread_fast_rng());

                        potential_rpcs.extend(more_rpcs.into_iter());

                        if potential_rpcs.len() >= self.min_head_rpcs {
                            // we have enough potential rpcs. try to load balance
                            match self
                                ._best_available_rpc(authorization, &potential_rpcs, skip_rpcs)
                                .await
                            {
                                OpenRequestResult::Handle(x) => {
                                    return Ok(OpenRequestResult::Handle(x))
                                }
                                OpenRequestResult::NotReady => {}
                                OpenRequestResult::RetryAt(retry_at) => {
                                    if earliest_retry_at.is_none() {
                                        earliest_retry_at = Some(retry_at);
                                    } else {
                                        earliest_retry_at = earliest_retry_at.min(Some(retry_at));
                                    }
                                }
                            }

                            // these rpcs were tried. don't try them again
                            potential_rpcs.clear();
                        }
                    }

                    if !potential_rpcs.is_empty() {
                        // even after scanning all the tiers, there are not enough rpcs that can serve this request. try anyways
                        match self
                            ._best_available_rpc(authorization, &potential_rpcs, skip_rpcs)
                            .await
                        {
                            OpenRequestResult::Handle(x) => {
                                return Ok(OpenRequestResult::Handle(x))
                            }
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

                    match consensus_rpcs.should_wait_for_block(waiting_for, skip_rpcs) {
                        ShouldWaitForBlock::NeverReady => break,
                        ShouldWaitForBlock::Ready => {
                            if Instant::now() > stop_trying_at {
                                break;
                            }
                        }
                        ShouldWaitForBlock::Wait { .. } => select! {
                            _ = watch_consensus_rpcs.changed() => {},
                            _ = sleep_until(stop_trying_at) => break,
                        },
                    }
                } else {
                    select! {
                        _ = watch_consensus_rpcs.changed() => {},
                        _ = sleep_until(stop_trying_at) => break,
                    }
                }
            }
        }

        if let Some(request_metadata) = request_metadata {
            request_metadata.no_servers.fetch_add(1, Ordering::AcqRel);
        }

        if let Some(retry_at) = earliest_retry_at {
            // TODO: log the server that retry_at came from
            warn!(
                "no servers in {} ready! Skipped {:?}. Retry in {:?}s",
                self,
                skip_rpcs,
                retry_at.duration_since(Instant::now()).as_secs_f32()
            );

            Ok(OpenRequestResult::RetryAt(retry_at))
        } else {
            warn!("no servers in {} ready! Skipped {:?}", self, skip_rpcs);

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
        authorization: &Arc<Authorization>,
        min_block_needed: Option<&U64>,
        max_block_needed: Option<&U64>,
        max_count: Option<usize>,
        allow_backups: bool,
    ) -> Result<Vec<OpenRequestHandle>, Option<Instant>> {
        let mut earliest_retry_at = None;

        let mut max_count = if let Some(max_count) = max_count {
            max_count
        } else {
            self.by_name.load().len()
        };

        trace!("max_count: {}", max_count);

        let mut selected_rpcs = Vec::with_capacity(max_count);

        let mut tried = HashSet::new();

        let mut synced_rpcs = {
            let synced_rpcs = self.watch_consensus_rpcs_sender.borrow();

            if let Some(synced_rpcs) = synced_rpcs.as_ref() {
                synced_rpcs.head_rpcs.clone()
            } else {
                vec![]
            }
        };

        // synced connections are all on the same block. sort them by tier with higher soft limits first
        synced_rpcs.sort_by_cached_key(rpc_sync_status_sort_key);

        trace!("synced_rpcs: {:#?}", synced_rpcs);

        // if there aren't enough synced connections, include more connections
        // TODO: only do this sorting if the synced_rpcs isn't enough
        let mut all_rpcs: Vec<_> = self.by_name.load().values().cloned().collect();
        all_rpcs.sort_by_cached_key(rpc_sync_status_sort_key);

        trace!("all_rpcs: {:#?}", all_rpcs);

        for rpc in itertools::chain(synced_rpcs, all_rpcs) {
            if max_count == 0 {
                break;
            }

            if tried.contains(&rpc) {
                continue;
            }

            trace!("trying {}", rpc);

            tried.insert(rpc.clone());

            if !allow_backups && rpc.backup {
                warn!("{} is a backup. skipping", rpc);
                continue;
            }

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
            match rpc.try_request_handle(authorization).await {
                Ok(OpenRequestResult::RetryAt(retry_at)) => {
                    // this rpc is not available. skip it
                    trace!("{} is rate limited. skipping", rpc);
                    earliest_retry_at = earliest_retry_at.min(Some(retry_at));
                }
                Ok(OpenRequestResult::Handle(handle)) => {
                    trace!("{} is available", rpc);
                    max_count -= 1;
                    selected_rpcs.push(handle)
                }
                Ok(OpenRequestResult::NotReady) => {
                    warn!("no request handle for {}", rpc)
                }
                Err(err) => {
                    warn!("error getting request handle for {}. err={:?}", rpc, err)
                }
            }
        }

        if !selected_rpcs.is_empty() {
            return Ok(selected_rpcs);
        }

        // return the earliest retry_after (if no rpcs are synced, this will be None)
        Err(earliest_retry_at)
    }

    /// be sure there is a timeout on this or it might loop forever
    /// TODO: think more about wait_for_sync
    pub async fn try_send_best_connection(
        &self,
        authorization: &Arc<Authorization>,
        request: &JsonRpcRequest,
        request_metadata: Option<&Arc<RequestMetadata>>,
        min_block_needed: Option<&U64>,
        max_block_needed: Option<&U64>,
    ) -> Web3ProxyResult<JsonRpcResponseData> {
        let mut skip_rpcs = vec![];
        let mut method_not_available_response = None;

        let mut watch_consensus_rpcs = self.watch_consensus_rpcs_sender.subscribe();

        let start = Instant::now();

        // TODO: get from config
        let max_wait = Duration::from_secs(10);

        // TODO: the loop here feels somewhat redundant with the loop in best_available_rpc
        while start.elapsed() < max_wait {
            match self
                .wait_for_best_rpc(
                    authorization,
                    request_metadata,
                    &mut skip_rpcs,
                    min_block_needed,
                    max_block_needed,
                    None,
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

                    // TODO: get the log percent from the user data
                    let response_result: Result<Box<RawValue>, _> = active_request_handle
                        .request(
                            &request.method,
                            &json!(request.params),
                            RequestErrorHandler::Save,
                        )
                        .await;

                    match response_result {
                        Ok(response) => {
                            // TODO: if there are multiple responses being aggregated, this will only use the last server's backup type
                            if let Some(request_metadata) = request_metadata {
                                request_metadata
                                    .response_from_backup_rpc
                                    .store(is_backup_response, Ordering::Release);
                            }

                            return Ok(response.into());
                        }
                        Err(error) => {
                            // trace!(?response, "rpc error");

                            // TODO: separate jsonrpc error and web3 proxy error!
                            if let Some(request_metadata) = request_metadata {
                                request_metadata
                                    .error_response
                                    .store(true, Ordering::Release);
                            }

                            let error: JsonRpcErrorData = error.try_into()?;

                            // some errors should be retried on other nodes
                            let error_msg = error.message.as_ref();

                            // different providers do different codes. check all of them
                            // TODO: there's probably more strings to add here
                            let rate_limit_substrings = ["limit", "exceeded", "quota usage"];
                            for rate_limit_substr in rate_limit_substrings {
                                if error_msg.contains(rate_limit_substr) {
                                    warn!(
                                        "rate limited ({}) by {}",
                                        error_msg,
                                        skip_rpcs.last().unwrap()
                                    );
                                    continue;
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

                            return Ok(error.into());
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
                    break;
                }
            }
        }

        // TODO: do we need this here, or do we do it somewhere else? like, the code could change and a try operator in here would skip this increment
        if let Some(request_metadata) = request_metadata {
            request_metadata
                .error_response
                .store(true, Ordering::Release);
        }

        if let Some(err) = method_not_available_response {
            // TODO: this error response is likely the user's fault. do we actually want it marked as an error? maybe keep user and server error bools?
            // TODO: emit a stat for unsupported methods? it would be best to block them at the proxy instead of at the backend
            // TODO: this is too verbose!
            debug!("{}", serde_json::to_string(&err)?);
            return Ok(err.into());
        }

        let num_conns = self.by_name.load().len();
        let num_skipped = skip_rpcs.len();

        let needed = min_block_needed.max(max_block_needed);

        let head_block_num = watch_consensus_rpcs
            .borrow()
            .as_ref()
            .map(|x| *x.head_block.number());

        // TODO: error? warn? debug? trace?
        if head_block_num.is_none() {
            error!(
                "No servers synced (min {:?}, max {:?}, head {:?}) ({} known)",
                min_block_needed, max_block_needed, head_block_num, num_conns
            );
        } else if head_block_num.as_ref() > needed {
            // we have synced past the needed block
            // TODO: this is likely caused by rate limits. make the error message better
            error!(
                "No archive servers synced (min {:?}, max {:?}, head {:?}) ({} known)",
                min_block_needed, max_block_needed, head_block_num, num_conns
            );
        } else {
            error!(
                "Requested data is not available (min {:?}, max {:?}, head {:?}) ({} skipped, {} known)",
                min_block_needed, max_block_needed, head_block_num, num_skipped, num_conns
            );
            // TODO: remove this, or move to trace level
            // debug!("{}", serde_json::to_string(&request).unwrap());
        }

        // TODO: what error code?
        // cloudflare gives {"jsonrpc":"2.0","error":{"code":-32043,"message":"Requested data cannot be older than 128 blocks."},"id":1}
        Ok(JsonRpcErrorData {
            message: Cow::Borrowed("Requested data is not available"),
            code: -32043,
            data: None,
        }
        .into())
    }

    /// be sure there is a timeout on this or it might loop forever
    #[allow(clippy::too_many_arguments)]
    pub async fn try_send_all_synced_connections(
        self: &Arc<Self>,
        authorization: &Arc<Authorization>,
        request: &JsonRpcRequest,
        request_metadata: Option<Arc<RequestMetadata>>,
        min_block_needed: Option<&U64>,
        max_block_needed: Option<&U64>,
        error_level: Level,
        max_count: Option<usize>,
        always_include_backups: bool,
    ) -> Web3ProxyResult<JsonRpcResponseData> {
        let mut watch_consensus_rpcs = self.watch_consensus_rpcs_sender.subscribe();

        let start = Instant::now();

        // TODO: get from config
        let max_wait = Duration::from_secs(3);

        while start.elapsed() < max_wait {
            match self
                .all_connections(
                    authorization,
                    min_block_needed,
                    max_block_needed,
                    max_count,
                    always_include_backups,
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

                                x.clone_connection()
                            }),
                        );

                        request_metadata
                            .response_from_backup_rpc
                            .store(only_backups_used, Ordering::Release);
                    }

                    return self
                        .try_send_parallel_requests(
                            active_request_handles,
                            request.method.as_ref(),
                            request.params.as_ref(),
                            error_level,
                        )
                        .await;
                }
                Err(None) => {
                    warn!(
                        "No servers in sync on {:?} (block {:?} - {:?})! Retrying",
                        self, min_block_needed, max_block_needed
                    );

                    if let Some(request_metadata) = &request_metadata {
                        // TODO: if this times out, i think we drop this
                        request_metadata.no_servers.fetch_add(1, Ordering::AcqRel);
                    }

                    watch_consensus_rpcs.changed().await?;
                    watch_consensus_rpcs.borrow_and_update();

                    continue;
                }
                Err(Some(retry_at)) => {
                    // TODO: move this to a helper function
                    // sleep (TODO: with a lock?) until our rate limits should be available
                    // TODO: if a server catches up sync while we are waiting, we could stop waiting
                    warn!("All rate limits exceeded. Sleeping");

                    if let Some(request_metadata) = &request_metadata {
                        request_metadata.no_servers.fetch_add(1, Ordering::AcqRel);
                    }

                    tokio::select! {
                        _ = sleep_until(retry_at) => {}
                        _ = watch_consensus_rpcs.changed() => {
                            watch_consensus_rpcs.borrow_and_update();
                        }
                    }

                    continue;
                }
            }
        }

        Err(Web3ProxyError::NoServersSynced)
    }

    pub async fn try_proxy_connection(
        &self,
        authorization: &Arc<Authorization>,
        request: &JsonRpcRequest,
        request_metadata: Option<&Arc<RequestMetadata>>,
        min_block_needed: Option<&U64>,
        max_block_needed: Option<&U64>,
    ) -> Web3ProxyResult<JsonRpcResponseData> {
        match authorization.checks.proxy_mode {
            ProxyMode::Debug | ProxyMode::Best => {
                self.try_send_best_connection(
                    authorization,
                    request,
                    request_metadata,
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
        f.debug_struct("Web3Rpcs")
            .field("rpcs", &self.by_name)
            .finish_non_exhaustive()
    }
}

impl Serialize for Web3Rpcs {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("Web3Rpcs", 1)?;

        {
            let by_name = self.by_name.load();
            let rpcs: Vec<&Web3Rpc> = by_name.values().map(|x| x.as_ref()).collect();
            // TODO: coordinate with frontend team to rename "conns" to "rpcs"
            state.serialize_field("conns", &rpcs)?;
        }

        {
            let consensus_rpcs = self.watch_consensus_rpcs_sender.borrow().clone();
            // TODO: rename synced_connections to consensus_rpcs

            if let Some(consensus_rpcs) = consensus_rpcs.as_ref() {
                state.serialize_field("synced_connections", consensus_rpcs)?;
            } else {
                state.serialize_field("synced_connections", &None::<()>)?;
            }
        }

        // self.blocks_by_hash.sync();
        // self.blocks_by_number.sync();
        // state.serialize_field("block_hashes_count", &self.blocks_by_hash.entry_count())?;
        // state.serialize_field("block_hashes_size", &self.blocks_by_hash.weighted_size())?;
        // state.serialize_field("block_numbers_count", &self.blocks_by_number.entry_count())?;
        // state.serialize_field("block_numbers_size", &self.blocks_by_number.weighted_size())?;
        state.end()
    }
}

/// sort by block number (descending) and tier (ascending)
/// TODO: should this be moved into a `impl Web3Rpc`?
/// TODO: i think we still have sorts scattered around the code that should use this
/// TODO: take AsRef or something like that? We don't need an Arc here
fn rpc_sync_status_sort_key(x: &Arc<Web3Rpc>) -> (Reverse<U64>, u64, bool, OrderedFloat<f64>) {
    let head_block = x
        .head_block
        .as_ref()
        .and_then(|x| x.borrow().as_ref().map(|x| *x.number()))
        .unwrap_or_default();

    let tier = x.tier;

    let peak_ewma = x.peak_ewma();

    let backup = x.backup;

    (Reverse(head_block), tier, backup, peak_ewma)
}

mod tests {
    #![allow(unused_imports)]

    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::rpcs::blockchain::Web3ProxyBlock;
    use crate::rpcs::consensus::ConsensusFinder;
    use arc_swap::ArcSwap;
    use ethers::types::H256;
    use ethers::types::{Block, U256};
    use latency::PeakEwmaLatency;
    use log::{trace, LevelFilter};
    use parking_lot::RwLock;
    use tokio::sync::RwLock as AsyncRwLock;

    #[cfg(test)]
    fn new_peak_latency() -> PeakEwmaLatency {
        PeakEwmaLatency::spawn(Duration::from_secs(1), 4, Duration::from_secs(1))
    }

    #[tokio::test]
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
                tier: 0,
                head_block: Some(tx_a),
                peak_latency: Some(new_peak_latency()),
                ..Default::default()
            },
            Web3Rpc {
                name: "b".to_string(),
                tier: 0,
                head_block: Some(tx_b),
                peak_latency: Some(new_peak_latency()),
                ..Default::default()
            },
            Web3Rpc {
                name: "c".to_string(),
                tier: 0,
                head_block: Some(tx_c),
                peak_latency: Some(new_peak_latency()),
                ..Default::default()
            },
            Web3Rpc {
                name: "d".to_string(),
                tier: 1,
                head_block: Some(tx_d),
                peak_latency: Some(new_peak_latency()),
                ..Default::default()
            },
            Web3Rpc {
                name: "e".to_string(),
                tier: 1,
                head_block: Some(tx_e),
                peak_latency: Some(new_peak_latency()),
                ..Default::default()
            },
            Web3Rpc {
                name: "f".to_string(),
                tier: 1,
                head_block: Some(tx_f),
                peak_latency: Some(new_peak_latency()),
                ..Default::default()
            },
        ]
        .into_iter()
        .map(Arc::new)
        .collect();

        rpcs.sort_by_cached_key(rpc_sync_status_sort_key);

        let names_in_sort_order: Vec<_> = rpcs.iter().map(|x| x.name.as_str()).collect();

        assert_eq!(names_in_sort_order, ["c", "f", "b", "e", "a", "d"]);
    }

    #[tokio::test]
    async fn test_server_selection_by_height() {
        // TODO: do this better. can test_env_logger and tokio test be stacked?
        let _ = env_logger::builder()
            .filter_level(LevelFilter::Error)
            .filter_module("web3_proxy", LevelFilter::Trace)
            .is_test(true)
            .try_init();

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
            tier: 0,
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
            tier: 0,
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

        let rpcs_by_name = HashMap::from([
            (head_rpc.name.clone(), head_rpc.clone()),
            (lagged_rpc.name.clone(), lagged_rpc.clone()),
        ]);

        let (block_sender, _block_receiver) = flume::unbounded();
        let (pending_tx_id_sender, pending_tx_id_receiver) = flume::unbounded();
        let (watch_consensus_rpcs_sender, _watch_consensus_rpcs_receiver) = watch::channel(None);
        let (watch_consensus_head_sender, _watch_consensus_head_receiver) = watch::channel(None);

        // TODO: make a Web3Rpcs::new
        let rpcs = Web3Rpcs {
            block_sender: block_sender.clone(),
            by_name: ArcSwap::from_pointee(rpcs_by_name),
            name: "test".to_string(),
            watch_consensus_head_sender: Some(watch_consensus_head_sender),
            watch_consensus_rpcs_sender,
            pending_transaction_cache: CacheWithTTL::arc_with_capacity(
                "pending_transaction_cache",
                100,
                Duration::from_secs(60),
            )
            .await,
            pending_tx_id_receiver,
            pending_tx_id_sender,
            blocks_by_hash: CacheWithTTL::arc_with_capacity(
                "blocks_by_hash",
                100,
                Duration::from_secs(60),
            )
            .await,
            blocks_by_number: CacheWithTTL::arc_with_capacity(
                "blocks_by_number",
                100,
                Duration::from_secs(60),
            )
            .await,
            // TODO: test max_block_age?
            max_block_age: None,
            // TODO: test max_block_lag?
            max_block_lag: None,
            min_head_rpcs: 1,
            min_sum_soft_limit: 1,
        };

        let authorization = Arc::new(Authorization::internal(None).unwrap());

        let mut consensus_finder = ConsensusFinder::new(None, None);

        // process None so that
        rpcs.process_block_from_rpc(
            &authorization,
            &mut consensus_finder,
            None,
            lagged_rpc.clone(),
            &None,
        )
        .await
        .expect("its lagged, but it should still be seen as consensus if its the first to report");
        rpcs.process_block_from_rpc(
            &authorization,
            &mut consensus_finder,
            None,
            head_rpc.clone(),
            &None,
        )
        .await
        .unwrap();

        // no head block because the rpcs haven't communicated through their channels
        assert!(rpcs.head_block_hash().is_none());

        // all_backend_connections gives all non-backup servers regardless of sync status
        assert_eq!(
            rpcs.all_connections(&authorization, None, None, None, false)
                .await
                .unwrap()
                .len(),
            2
        );

        // best_synced_backend_connection which servers to be synced with the head block should not find any nodes
        let x = rpcs
            .wait_for_best_rpc(
                &authorization,
                None,
                &mut vec![],
                Some(head_block.number.as_ref().unwrap()),
                None,
                Some(Duration::from_secs(0)),
            )
            .await
            .unwrap();

        dbg!(&x);

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

        // TODO: this is fragile
        rpcs.process_block_from_rpc(
            &authorization,
            &mut consensus_finder,
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
        rpcs.process_block_from_rpc(
            &authorization,
            &mut consensus_finder,
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
        rpcs.process_block_from_rpc(
            &authorization,
            &mut consensus_finder,
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
                &authorization,
                None,
                &mut vec![],
                None,
                None,
                Some(Duration::from_secs(0))
            )
            .await,
            Ok(OpenRequestResult::Handle(_))
        ));

        // TODO: make sure the handle is for the expected rpc
        assert!(matches!(
            rpcs.wait_for_best_rpc(
                &authorization,
                None,
                &mut vec![],
                Some(&0.into()),
                None,
                Some(Duration::from_secs(0)),
            )
            .await,
            Ok(OpenRequestResult::Handle(_))
        ));

        // TODO: make sure the handle is for the expected rpc
        assert!(matches!(
            rpcs.wait_for_best_rpc(
                &authorization,
                None,
                &mut vec![],
                Some(&1.into()),
                None,
                Some(Duration::from_secs(0)),
            )
            .await,
            Ok(OpenRequestResult::Handle(_))
        ));

        // future block should not get a handle
        let future_rpc = rpcs
            .wait_for_best_rpc(
                &authorization,
                None,
                &mut vec![],
                Some(&2.into()),
                None,
                Some(Duration::from_secs(0)),
            )
            .await;
        assert!(matches!(future_rpc, Ok(OpenRequestResult::NotReady)));
    }

    #[tokio::test]
    async fn test_server_selection_by_archive() {
        // TODO: do this better. can test_env_logger and tokio test be stacked?
        let _ = env_logger::builder()
            .filter_level(LevelFilter::Error)
            .filter_module("web3_proxy", LevelFilter::Trace)
            .is_test(true)
            .try_init();

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
            tier: 1,
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
            tier: 2,
            head_block: Some(tx_archive),
            ..Default::default()
        };

        assert!(pruned_rpc.has_block_data(head_block.number()));
        assert!(archive_rpc.has_block_data(head_block.number()));
        assert!(!pruned_rpc.has_block_data(&1.into()));
        assert!(archive_rpc.has_block_data(&1.into()));

        let pruned_rpc = Arc::new(pruned_rpc);
        let archive_rpc = Arc::new(archive_rpc);

        let rpcs_by_name = HashMap::from([
            (pruned_rpc.name.clone(), pruned_rpc.clone()),
            (archive_rpc.name.clone(), archive_rpc.clone()),
        ]);

        let (block_sender, _) = flume::unbounded();
        let (pending_tx_id_sender, pending_tx_id_receiver) = flume::unbounded();
        let (watch_consensus_rpcs_sender, _) = watch::channel(None);
        let (watch_consensus_head_sender, _watch_consensus_head_receiver) = watch::channel(None);

        let rpcs = Web3Rpcs {
            block_sender,
            by_name: ArcSwap::from_pointee(rpcs_by_name),
            name: "test".to_string(),
            watch_consensus_head_sender: Some(watch_consensus_head_sender),
            watch_consensus_rpcs_sender,
            pending_transaction_cache: CacheWithTTL::arc_with_capacity(
                "pending_transaction_cache",
                100,
                Duration::from_secs(120),
            )
            .await,
            pending_tx_id_receiver,
            pending_tx_id_sender,
            blocks_by_hash: CacheWithTTL::arc_with_capacity(
                "blocks_by_hash",
                100,
                Duration::from_secs(120),
            )
            .await,
            blocks_by_number: CacheWithTTL::arc_with_capacity(
                "blocks_by_number",
                100,
                Duration::from_secs(120),
            )
            .await,
            min_head_rpcs: 1,
            min_sum_soft_limit: 4_000,
            max_block_age: None,
            max_block_lag: None,
        };

        let authorization = Arc::new(Authorization::internal(None).unwrap());

        let mut connection_heads = ConsensusFinder::new(None, None);

        // min sum soft limit will require tier 2
        rpcs.process_block_from_rpc(
            &authorization,
            &mut connection_heads,
            Some(head_block.clone()),
            pruned_rpc.clone(),
            &None,
        )
        .await
        .unwrap_err();

        rpcs.process_block_from_rpc(
            &authorization,
            &mut connection_heads,
            Some(head_block.clone()),
            archive_rpc.clone(),
            &None,
        )
        .await
        .unwrap();

        assert_eq!(rpcs.num_synced_rpcs(), 2);

        // best_synced_backend_connection requires servers to be synced with the head block
        // TODO: test with and without passing the head_block.number?
        let best_available_server = rpcs
            .wait_for_best_rpc(
                &authorization,
                None,
                &mut vec![],
                Some(head_block.number()),
                None,
                Some(Duration::from_secs(0)),
            )
            .await;

        debug!("best_available_server: {:#?}", best_available_server);

        assert!(matches!(
            best_available_server.unwrap(),
            OpenRequestResult::Handle(_)
        ));

        let _best_available_server_from_none = rpcs
            .wait_for_best_rpc(
                &authorization,
                None,
                &mut vec![],
                None,
                None,
                Some(Duration::from_secs(0)),
            )
            .await;

        // assert_eq!(best_available_server, best_available_server_from_none);

        let best_archive_server = rpcs
            .wait_for_best_rpc(
                &authorization,
                None,
                &mut vec![],
                Some(&1.into()),
                None,
                Some(Duration::from_secs(0)),
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

    #[tokio::test]
    async fn test_all_connections() {
        let _ = env_logger::builder()
            .filter_level(LevelFilter::Error)
            .filter_module("web3_proxy", LevelFilter::Trace)
            .is_test(true)
            .try_init();

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
            tier: 0,
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
            tier: 1,
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

        let rpcs_by_name = HashMap::from([
            (mock_geth.name.clone(), mock_geth.clone()),
            (
                mock_erigon_archive.name.clone(),
                mock_erigon_archive.clone(),
            ),
        ]);

        let (block_sender, _) = flume::unbounded();
        let (pending_tx_id_sender, pending_tx_id_receiver) = flume::unbounded();
        let (watch_consensus_rpcs_sender, _) = watch::channel(None);
        let (watch_consensus_head_sender, _watch_consensus_head_receiver) = watch::channel(None);

        // TODO: make a Web3Rpcs::new
        let rpcs = Web3Rpcs {
            block_sender,
            by_name: ArcSwap::from_pointee(rpcs_by_name),
            name: "test".to_string(),
            watch_consensus_head_sender: Some(watch_consensus_head_sender),
            watch_consensus_rpcs_sender,
            pending_transaction_cache: CacheWithTTL::arc_with_capacity(
                "pending_transaction_cache",
                10_000,
                Duration::from_secs(120),
            )
            .await,
            pending_tx_id_receiver,
            pending_tx_id_sender,
            blocks_by_hash: CacheWithTTL::arc_with_capacity(
                "blocks_by_hash",
                10_000,
                Duration::from_secs(120),
            )
            .await,
            blocks_by_number: CacheWithTTL::arc_with_capacity(
                "blocks_by_number",
                10_000,
                Duration::from_secs(120),
            )
            .await,
            min_head_rpcs: 1,
            min_sum_soft_limit: 1_000,
            max_block_age: None,
            max_block_lag: None,
        };

        let authorization = Arc::new(Authorization::internal(None).unwrap());

        let mut connection_heads = ConsensusFinder::new(None, None);

        rpcs.process_block_from_rpc(
            &authorization,
            &mut connection_heads,
            Some(block_1.clone()),
            mock_geth.clone(),
            &None,
        )
        .await
        .unwrap();

        rpcs.process_block_from_rpc(
            &authorization,
            &mut connection_heads,
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
            .all_connections(&authorization, Some(block_2.number()), None, None, false)
            .await;

        debug!("head_connections: {:#?}", head_connections);

        assert_eq!(
            head_connections.unwrap().len(),
            1,
            "wrong number of connections"
        );

        let all_connections = rpcs
            .all_connections(&authorization, Some(block_1.number()), None, None, false)
            .await;

        debug!("all_connections: {:#?}", all_connections);

        assert_eq!(
            all_connections.unwrap().len(),
            2,
            "wrong number of connections"
        );

        let all_connections = rpcs
            .all_connections(&authorization, None, None, None, false)
            .await;

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
