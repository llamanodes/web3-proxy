///! Load balanced communication with a group of web3 rpc providers
use super::blockchain::{BlocksByHashCache, Web3ProxyBlock};
use super::consensus::ConsensusWeb3Rpcs;
use super::one::Web3Rpc;
use super::request::{OpenRequestHandle, OpenRequestResult, RequestErrorHandler};
use crate::app::{flatten_handle, AnyhowJoinHandle, Web3ProxyApp};
///! Load balanced communication with a group of web3 providers
use crate::config::{BlockAndRpc, TxHashAndRpc, Web3RpcConfig};
use crate::frontend::authorization::{Authorization, RequestMetadata};
use crate::frontend::errors::{Web3ProxyError, Web3ProxyResult};
use crate::frontend::rpc_proxy_ws::ProxyMode;
use crate::jsonrpc::{JsonRpcForwardedResponse, JsonRpcRequest};
use crate::rpcs::transactions::TxStatus;
use anyhow::Context;
use arc_swap::ArcSwap;
use counter::Counter;
use derive_more::From;
use ethers::prelude::{ProviderError, TxHash, H256, U64};
use futures::future::try_join_all;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use hashbrown::{HashMap, HashSet};
use itertools::Itertools;
use log::{debug, error, info, trace, warn, Level};
use migration::sea_orm::DatabaseConnection;
use moka::future::{Cache, ConcurrentCacheExt};
use ordered_float::OrderedFloat;
use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;
use serde_json::json;
use serde_json::value::RawValue;
use std::cmp::{min_by_key, Reverse};
use std::collections::BTreeMap;
use std::sync::atomic::{self, Ordering};
use std::sync::Arc;
use std::{cmp, fmt};
use thread_fast_rng::rand::seq::SliceRandom;
use tokio;
use tokio::sync::{broadcast, watch};
use tokio::time::{interval, sleep, sleep_until, Duration, Instant, MissedTickBehavior};

/// A collection of web3 connections. Sends requests either the current best server or all servers.
#[derive(From)]
pub struct Web3Rpcs {
    /// if watch_consensus_head_sender is some, Web3Rpc inside self will send blocks here when they get them
    pub(crate) block_sender: flume::Sender<(Option<Web3ProxyBlock>, Arc<Web3Rpc>)>,
    /// any requests will be forwarded to one (or more) of these connections
    pub(crate) by_name: ArcSwap<HashMap<String, Arc<Web3Rpc>>>,
    /// notify all http providers to check their blocks at the same time
    pub(crate) http_interval_sender: Option<Arc<broadcast::Sender<()>>>,
    /// all providers with the same consensus head block. won't update if there is no `self.watch_consensus_head_sender`
    /// TODO: document that this is a watch sender and not a broadcast! if things get busy, blocks might get missed
    /// TODO: why is watch_consensus_head_sender in an Option, but this one isn't?
    /// Geth's subscriptions have the same potential for skipping blocks.
    pub(crate) watch_consensus_rpcs_sender: watch::Sender<Option<Arc<ConsensusWeb3Rpcs>>>,
    /// this head receiver makes it easy to wait until there is a new block
    pub(super) watch_consensus_head_sender: Option<watch::Sender<Option<Web3ProxyBlock>>>,
    pub(super) pending_transaction_cache:
        Cache<TxHash, TxStatus, hashbrown::hash_map::DefaultHashBuilder>,
    pub(super) pending_tx_id_receiver: flume::Receiver<TxHashAndRpc>,
    pub(super) pending_tx_id_sender: flume::Sender<TxHashAndRpc>,
    /// TODO: this map is going to grow forever unless we do some sort of pruning. maybe store pruned in redis?
    /// all blocks, including orphans
    pub(super) blocks_by_hash: BlocksByHashCache,
    /// blocks on the heaviest chain
    pub(super) blocks_by_number: Cache<U64, H256, hashbrown::hash_map::DefaultHashBuilder>,
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
        chain_id: u64,
        db_conn: Option<DatabaseConnection>,
        http_client: Option<reqwest::Client>,
        max_block_age: Option<u64>,
        max_block_lag: Option<U64>,
        min_head_rpcs: usize,
        min_sum_soft_limit: u32,
        pending_transaction_cache: Cache<TxHash, TxStatus, hashbrown::hash_map::DefaultHashBuilder>,
        pending_tx_sender: Option<broadcast::Sender<TxStatus>>,
        watch_consensus_head_sender: Option<watch::Sender<Option<Web3ProxyBlock>>>,
    ) -> anyhow::Result<(
        Arc<Self>,
        AnyhowJoinHandle<()>,
        watch::Receiver<Option<Arc<ConsensusWeb3Rpcs>>>,
        // watch::Receiver<Arc<ConsensusWeb3Rpcs>>,
    )> {
        let (pending_tx_id_sender, pending_tx_id_receiver) = flume::unbounded();
        let (block_sender, block_receiver) = flume::unbounded::<BlockAndRpc>();

        // TODO: query the rpc to get the actual expected block time, or get from config? maybe have this be part of a health check?
        let expected_block_time_ms = match chain_id {
            // ethereum
            1 => 12_000,
            // ethereum-goerli
            5 => 12_000,
            // polygon
            137 => 2_000,
            // fantom
            250 => 1_000,
            // arbitrum
            42161 => 500,
            // anything else
            _ => {
                warn!(
                    "unexpected chain_id ({}). polling every {} seconds",
                    chain_id, 10
                );
                10_000
            }
        };

        let http_interval_sender = if http_client.is_some() {
            let (sender, _) = broadcast::channel(1);

            // TODO: what interval? follow a websocket also? maybe by watching synced connections with a timeout. will need debounce
            let mut interval = interval(Duration::from_millis(expected_block_time_ms / 2));
            interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

            let sender = Arc::new(sender);

            let f = {
                let sender = sender.clone();

                async move {
                    loop {
                        interval.tick().await;

                        // trace!("http interval ready");

                        if sender.send(()).is_err() {
                            // errors are okay. they mean that all receivers have been dropped, or the rpcs just haven't started yet
                            // TODO: i'm seeing this error a lot more than expected
                            trace!("no http receivers");
                        };
                    }
                }
            };

            // TODO: do something with this handle?
            tokio::spawn(f);

            Some(sender)
        } else {
            None
        };

        // these blocks don't have full transactions, but they do have rather variable amounts of transaction hashes
        // TODO: how can we do the weigher better? need to know actual allocated size
        // TODO: time_to_idle instead?
        // TODO: limits from config
        let blocks_by_hash: BlocksByHashCache = Cache::builder()
            .max_capacity(1024 * 1024 * 1024)
            .weigher(|_k, v: &Web3ProxyBlock| {
                1 + v.block.transactions.len().try_into().unwrap_or(u32::MAX)
            })
            .time_to_live(Duration::from_secs(30 * 60))
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default());

        // all block numbers are the same size, so no need for weigher
        // TODO: limits from config
        // TODO: time_to_idle instead?
        let blocks_by_number = Cache::builder()
            .time_to_live(Duration::from_secs(30 * 60))
            .max_capacity(10_000)
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default());

        let (watch_consensus_rpcs_sender, consensus_connections_watcher) =
            watch::channel(Default::default());

        // by_name starts empty. self.apply_server_configs will add to it
        let by_name = Default::default();

        let connections = Arc::new(Self {
            block_sender,
            by_name,
            http_interval_sender,
            watch_consensus_rpcs_sender,
            watch_consensus_head_sender,
            pending_transaction_cache,
            pending_tx_id_sender,
            pending_tx_id_receiver,
            blocks_by_hash,
            blocks_by_number,
            min_sum_soft_limit,
            min_head_rpcs,
            max_block_age,
            max_block_lag,
        });

        let authorization = Arc::new(Authorization::internal(db_conn)?);

        let handle = {
            let connections = connections.clone();

            tokio::spawn(async move {
                connections
                    .subscribe(authorization, block_receiver, pending_tx_sender)
                    .await
            })
        };

        Ok((connections, handle, consensus_connections_watcher))
    }

    /// update the rpcs in this group
    pub async fn apply_server_configs(
        &self,
        app: &Web3ProxyApp,
        rpc_configs: HashMap<String, Web3RpcConfig>,
    ) -> anyhow::Result<()> {
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

        // TODO: < is a bit dangerous, we should require a buffer
        if sum_soft_limit < self.min_sum_soft_limit {
            return Err(anyhow::anyhow!(
                "Only {}/{} soft limit! Add more rpcs, increase soft limits, or reduce min_sum_soft_limit.",
                sum_soft_limit,
                self.min_sum_soft_limit
            ));
        }

        // turn configs into connections (in parallel)
        // TODO: move this into a helper function. then we can use it when configs change (will need a remove function too)
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
                let blocks_by_hash = self.blocks_by_hash.clone();
                let http_interval_sender = self.http_interval_sender.clone();
                let chain_id = app.config.chain_id;

                debug!("spawning {}", server_name);

                let handle = tokio::spawn(async move {
                    server_config
                        .spawn(
                            server_name,
                            db_conn,
                            vredis_pool,
                            chain_id,
                            http_client,
                            http_interval_sender,
                            blocks_by_hash,
                            block_sender,
                            pending_tx_id_sender,
                            true,
                        )
                        .await
                });

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
                        }

                        old_rpc.disconnect().await.context("disconnect old rpc")?;
                    }

                    // TODO: what should we do with the new handle? make sure error logs aren't dropped
                }
                Ok(Err(err)) => {
                    // if we got an error here, the app can continue on
                    // TODO: include context about which connection failed
                    // TODO: will this retry automatically? i don't think so
                    error!("Unable to create connection. err={:?}", err);
                }
                Err(err) => {
                    // something actually bad happened. exit with an error
                    return Err(err.into());
                }
            }
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
    ) -> anyhow::Result<()> {
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
                        // TODO: "every interval, check that the provider is still connected"
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
        id: Box<RawValue>,
        method: &str,
        params: Option<&serde_json::Value>,
        error_level: Level,
        // TODO: remove this box once i figure out how to do the options
    ) -> Web3ProxyResult<JsonRpcForwardedResponse> {
        // TODO: if only 1 active_request_handles, do self.try_send_request?

        let responses = active_request_handles
            .into_iter()
            .map(|active_request_handle| async move {
                let result: Result<Box<RawValue>, _> = active_request_handle
                    .request(method, &json!(&params), error_level.into(), None)
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
        let mut any_ok_but_maybe_json_error = false;
        for partial_response in responses {
            if partial_response.is_ok() {
                any_ok_with_json_result = true;
            }

            let response =
                JsonRpcForwardedResponse::try_from_response_result(partial_response, id.clone());

            // TODO: better key?
            let s = format!("{:?}", response);

            if count_map.get(&s).is_none() {
                if response.is_ok() {
                    any_ok_but_maybe_json_error = true;
                }

                count_map.insert(s.clone(), response);
            }

            counts.update([s].into_iter());
        }

        for (most_common, _) in counts.most_common_ordered() {
            let most_common = count_map
                .remove(&most_common)
                .expect("most_common key must exist");

            match most_common {
                Ok(x) => {
                    if any_ok_with_json_result && x.error.is_some() {
                        // this one may be an "Ok", but the json has an error inside it
                        continue;
                    }
                    // return the most common success
                    return Ok(x);
                }
                Err(err) => {
                    if any_ok_but_maybe_json_error {
                        // the most common is an error, but there is an Ok in here somewhere. loop to find it
                        continue;
                    }
                    return Err(err);
                }
            }
        }

        // TODO: what should we do if we get here? i don't think we will
        unimplemented!("this shouldn't be possible")
    }

    pub async fn best_available_rpc(
        &self,
        authorization: &Arc<Authorization>,
        request_metadata: Option<&Arc<RequestMetadata>>,
        skip: &[Arc<Web3Rpc>],
        // TODO: if we are checking for the consensus head, i don' think we need min_block_needed/max_block_needed
        min_block_needed: Option<&U64>,
        max_block_needed: Option<&U64>,
    ) -> Web3ProxyResult<OpenRequestResult> {
        let usable_rpcs_by_tier_and_head_number: BTreeMap<
            (u64, Reverse<Option<U64>>),
            Vec<Arc<Web3Rpc>>,
        > = {
            let mut m = BTreeMap::new();

            if self.watch_consensus_head_sender.is_none() {
                // pick any server

                let key = (0, Reverse(None));

                for x in self.by_name.load().values() {
                    if skip.contains(x) {
                        trace!("skipping: {}", x);
                        continue;
                    }
                    trace!("not skipped!");

                    m.entry(key).or_insert_with(Vec::new).push(x.clone());
                }
            } else {
                let synced_connections = self.watch_consensus_rpcs_sender.borrow().clone();

                if synced_connections.is_none() {
                    return Ok(OpenRequestResult::NotReady);
                }
                let synced_connections =
                    synced_connections.expect("synced_connections can't be None here");

                let head_block_num = synced_connections.head_block.number();
                let head_block_age = synced_connections.head_block.age();

                // TODO: double check the logic on this. especially if only min is set
                let needed_blocks_comparison = match (min_block_needed, max_block_needed) {
                    (None, None) => {
                        // no required block given. treat this like they requested the consensus head block
                        cmp::Ordering::Equal
                    }
                    (None, Some(max_block_needed)) => max_block_needed.cmp(head_block_num),
                    (Some(min_block_needed), None) => min_block_needed.cmp(head_block_num),
                    (Some(min_block_needed), Some(max_block_needed)) => {
                        match min_block_needed.cmp(max_block_needed) {
                            cmp::Ordering::Less | cmp::Ordering::Equal => {
                                min_block_needed.cmp(head_block_num)
                            }
                            cmp::Ordering::Greater => {
                                // TODO: force a debug log of the original request to see if our logic is wrong?
                                // TODO: attach the rpc_key_id so we can find the user to ask if they need help
                                return Err(Web3ProxyError::InvalidBlockBounds {
                                    min: min_block_needed.as_u64(),
                                    max: max_block_needed.as_u64(),
                                });
                            }
                        }
                    }
                };

                trace!("needed_blocks_comparison: {:?}", needed_blocks_comparison);

                // collect "usable_rpcs_by_head_num_and_weight"
                match needed_blocks_comparison {
                    cmp::Ordering::Less => {
                        // need an old block. check all the rpcs. ignore rpcs that are still syncing
                        trace!("old block needed");

                        let min_block_age =
                            self.max_block_age.map(|x| head_block_age.saturating_sub(x));
                        let min_sync_num =
                            self.max_block_lag.map(|x| head_block_num.saturating_sub(x));

                        // TODO: cache this somehow?
                        // TODO: maybe have a helper on synced_connections? that way sum_soft_limits/min_synced_rpcs will be DRY
                        for x in self
                            .by_name
                            .load()
                            .values()
                            .filter(|x| {
                                // TODO: move a bunch of this onto a rpc.is_synced function
                                #[allow(clippy::if_same_then_else)]
                                if skip.contains(x) {
                                    // we've already tried this server or have some other reason to skip it
                                    false
                                } else if max_block_needed
                                    .map(|max_block_needed| !x.has_block_data(max_block_needed))
                                    .unwrap_or(false)
                                {
                                    // server does not have the max block
                                    trace!(
                                        "{} does not have the max block ({:?})",
                                        x,
                                        max_block_needed
                                    );
                                    false
                                } else {
                                    !min_block_needed
                                        .map(|min_block_needed| !x.has_block_data(min_block_needed))
                                        .unwrap_or(false)
                                }
                            })
                            .cloned()
                        {
                            let x_head_block = x.head_block.as_ref().unwrap().borrow().clone();

                            if let Some(x_head) = x_head_block {
                                // TODO: should nodes that are ahead of the consensus block have priority? seems better to spread the load
                                let x_head_num = x_head.number().min(head_block_num);

                                // TODO: do we really need to check head_num and age?
                                if let Some(min_sync_num) = min_sync_num.as_ref() {
                                    if x_head_num < min_sync_num {
                                        trace!("rpc is still syncing");
                                        continue;
                                    }
                                }
                                if let Some(min_block_age) = min_block_age {
                                    if x_head.age() > min_block_age {
                                        // rpc is still syncing
                                        trace!("server's block is too old");
                                        continue;
                                    }
                                }

                                let key = (x.tier, Reverse(Some(*x_head_num)));

                                m.entry(key).or_insert_with(Vec::new).push(x);
                            }
                        }

                        // TODO: check min_synced_rpcs and min_sum_soft_limits? or maybe better to just try to serve the request?
                    }
                    cmp::Ordering::Equal => {
                        // using the consensus head block. filter the synced rpcs

                        // the key doesn't matter if we are checking synced connections
                        // they are all at the same block and it is already sized to what we need
                        let key = (0, Reverse(None));

                        for x in synced_connections.best_rpcs.iter() {
                            if skip.contains(x) {
                                trace!("skipping: {}", x);
                                continue;
                            }
                            trace!("not skipped!");

                            m.entry(key).or_insert_with(Vec::new).push(x.clone());
                        }
                    }
                    cmp::Ordering::Greater => {
                        // TODO? if the blocks is close, maybe we could wait for change on a watch_consensus_connections_receiver().subscribe()
                        return Ok(OpenRequestResult::NotReady);
                    }
                }
            }

            m
        };

        trace!(
            "usable_rpcs_by_tier_and_head_number: {:#?}",
            usable_rpcs_by_tier_and_head_number
        );

        let mut earliest_retry_at = None;

        for mut usable_rpcs in usable_rpcs_by_tier_and_head_number.into_values() {
            // sort the tier randomly
            if usable_rpcs.len() == 1 {
                // TODO: include an rpc from the next tier?
            } else {
                // we can't get the rng outside of this loop because it is not Send
                // this function should be pretty fast anyway, so it shouldn't matter too much
                let mut rng = thread_fast_rng::thread_fast_rng();
                usable_rpcs.shuffle(&mut rng);
            };

            // now that the rpcs are shuffled, try to get an active request handle for one of them
            // pick the first two and try the one with the lower rpc.latency.ewma
            // TODO: chunks or tuple windows?
            for (rpc_a, rpc_b) in usable_rpcs.into_iter().circular_tuple_windows() {
                trace!("{} vs {}", rpc_a, rpc_b);
                // TODO: cached key to save a read lock
                // TODO: ties to the server with the smallest block_data_limit
                let best_rpc = min_by_key(rpc_a, rpc_b, |x| x.peak_ewma());
                trace!("winner: {}", best_rpc);

                // just because it has lower latency doesn't mean we are sure to get a connection
                match best_rpc.try_request_handle(authorization, None).await {
                    Ok(OpenRequestResult::Handle(handle)) => {
                        // trace!("opened handle: {}", best_rpc);
                        return Ok(OpenRequestResult::Handle(handle));
                    }
                    Ok(OpenRequestResult::RetryAt(retry_at)) => {
                        earliest_retry_at = earliest_retry_at.min(Some(retry_at));
                    }
                    Ok(OpenRequestResult::NotReady) => {
                        // TODO: log a warning? emit a stat?
                        trace!("best_rpc not ready: {}", best_rpc);
                    }
                    Err(err) => {
                        warn!("No request handle for {}. err={:?}", best_rpc, err)
                    }
                }
            }
        }

        if let Some(request_metadata) = request_metadata {
            request_metadata.no_servers.fetch_add(1, Ordering::Release);
        }

        match earliest_retry_at {
            None => {
                // none of the servers gave us a time to retry at

                // TODO: bring this back? need to think about how to do this with `allow_backups`
                // we could return an error here, but maybe waiting a second will fix the problem
                // TODO: configurable max wait? the whole max request time, or just some portion?
                // let handle = sorted_rpcs
                //     .get(0)
                //     .expect("at least 1 is available")
                //     .wait_for_request_handle(authorization, Duration::from_secs(3), false)
                //     .await?;
                // Ok(OpenRequestResult::Handle(handle))

                // TODO: should we log here?

                Ok(OpenRequestResult::NotReady)
            }
            Some(earliest_retry_at) => {
                warn!("no servers on {:?}! {:?}", self, earliest_retry_at);

                Ok(OpenRequestResult::RetryAt(earliest_retry_at))
            }
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
                synced_rpcs.best_rpcs.clone()
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
                    warn!("{} is missing min_block_needed. skipping", rpc);
                    continue;
                }
            }

            if let Some(block_needed) = max_block_needed {
                if !rpc.has_block_data(block_needed) {
                    warn!("{} is missing max_block_needed. skipping", rpc);
                    continue;
                }
            }

            // check rate limits and increment our connection counter
            match rpc.try_request_handle(authorization, None).await {
                Ok(OpenRequestResult::RetryAt(retry_at)) => {
                    // this rpc is not available. skip it
                    warn!("{} is rate limited. skipping", rpc);
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
    pub async fn try_send_best_consensus_head_connection(
        &self,
        authorization: &Arc<Authorization>,
        request: JsonRpcRequest,
        request_metadata: Option<&Arc<RequestMetadata>>,
        min_block_needed: Option<&U64>,
        max_block_needed: Option<&U64>,
    ) -> Web3ProxyResult<JsonRpcForwardedResponse> {
        let mut skip_rpcs = vec![];
        let mut method_not_available_response = None;

        let mut watch_consensus_connections = self.watch_consensus_rpcs_sender.subscribe();

        // TODO: maximum retries? right now its the total number of servers
        loop {
            if skip_rpcs.len() >= self.by_name.load().len() {
                break;
            }

            match self
                .best_available_rpc(
                    authorization,
                    request_metadata,
                    &skip_rpcs,
                    min_block_needed,
                    max_block_needed,
                )
                .await?
            {
                OpenRequestResult::Handle(active_request_handle) => {
                    // save the rpc in case we get an error and want to retry on another server
                    // TODO: look at backend_requests instead
                    let rpc = active_request_handle.clone_connection();
                    skip_rpcs.push(rpc.clone());

                    if let Some(request_metadata) = request_metadata {
                        request_metadata
                            .response_from_backup_rpc
                            .store(rpc.backup, Ordering::Release);

                        request_metadata.backend_requests.lock().push(rpc);
                    }

                    // TODO: get the log percent from the user data
                    let response_result = active_request_handle
                        .request(
                            &request.method,
                            &json!(request.params),
                            RequestErrorHandler::Save,
                            None,
                        )
                        .await;

                    match JsonRpcForwardedResponse::try_from_response_result(
                        response_result,
                        request.id.clone(),
                    ) {
                        Ok(response) => {
                            if let Some(error) = response.error.as_ref() {
                                // trace!(?response, "rpc error");

                                if let Some(request_metadata) = request_metadata {
                                    request_metadata
                                        .error_response
                                        .store(true, Ordering::Release);
                                }

                                // some errors should be retried on other nodes
                                let error_msg = error.message.as_str();

                                // different providers do different codes. check all of them
                                // TODO: there's probably more strings to add here
                                let rate_limit_substrings = ["limit", "exceeded"];
                                for rate_limit_substr in rate_limit_substrings {
                                    if error_msg.contains(rate_limit_substr) {
                                        warn!("rate limited by {}", skip_rpcs.last().unwrap());
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
                                                continue;
                                            }
                                        }
                                    }
                                    -32601 => {
                                        let error_msg = error.message.as_str();

                                        // sometimes a provider does not support all rpc methods
                                        // we check other connections rather than returning the error
                                        // but sometimes the method is something that is actually unsupported,
                                        // so we save the response here to return it later

                                        // some providers look like this
                                        if error_msg.starts_with("the method")
                                            && error_msg.ends_with("is not available")
                                        {
                                            method_not_available_response = Some(response);
                                            continue;
                                        }

                                        // others look like this (this is the example in the official spec)
                                        if error_msg == "Method not found" {
                                            method_not_available_response = Some(response);
                                            continue;
                                        }
                                    }
                                    _ => {}
                                }
                            } else {
                                // trace!(?response, "rpc success");
                            }

                            return Ok(response);
                        }
                        Err(err) => {
                            let rpc = skip_rpcs
                                .last()
                                .expect("there must have been a provider if we got an error");

                            // TODO: emit a stat. if a server is getting skipped a lot, something is not right

                            // TODO: if we get a TrySendError, reconnect. wait why do we see a trysenderror on a dual provider? shouldn't it be using reqwest

                            debug!(
                                "Backend server error on {}! Retrying on another. err={:#?}",
                                rpc, err
                            );

                            continue;
                        }
                    }
                }
                OpenRequestResult::RetryAt(retry_at) => {
                    // TODO: move this to a helper function
                    // sleep (TODO: with a lock?) until our rate limits should be available
                    // TODO: if a server catches up sync while we are waiting, we could stop waiting
                    warn!(
                        "All rate limits exceeded. waiting for change in synced servers or {:?}",
                        retry_at
                    );

                    // TODO: have a separate column for rate limited?
                    if let Some(request_metadata) = request_metadata {
                        request_metadata.no_servers.fetch_add(1, Ordering::Release);
                    }

                    tokio::select! {
                        _ = sleep_until(retry_at) => {
                            skip_rpcs.pop();
                        }
                        _ = watch_consensus_connections.changed() => {
                            watch_consensus_connections.borrow_and_update();
                        }
                    }
                }
                OpenRequestResult::NotReady => {
                    if let Some(request_metadata) = request_metadata {
                        request_metadata.no_servers.fetch_add(1, Ordering::Release);
                    }

                    let waiting_for = min_block_needed.max(max_block_needed);

                    if watch_for_block(waiting_for, &mut watch_consensus_connections).await? {
                        // block found! continue so we can check for another rpc
                        continue;
                    } else {
                        // block won't be found without new servers being added
                        break;
                    }
                }
            }
        }

        // TODO: do we need this here, or do we do it somewhere else? like, the code could change and a try operator in here would skip this increment
        if let Some(request_metadata) = request_metadata {
            request_metadata
                .error_response
                .store(true, Ordering::Release);
        }

        if let Some(r) = method_not_available_response {
            // TODO: this error response is likely the user's fault. do we actually want it marked as an error? maybe keep user and server error bools?
            // TODO: emit a stat for unsupported methods? it would be best to block them at the proxy instead of at the backend
            return Ok(r);
        }

        let num_conns = self.by_name.load().len();
        let num_skipped = skip_rpcs.len();

        let needed = min_block_needed.max(max_block_needed);

        let head_block_num = watch_consensus_connections
            .borrow()
            .as_ref()
            .map(|x| *x.head_block.number());

        // TODO: error? warn? debug? trace?
        if head_block_num.is_none() {
            error!(
                "No servers synced (min {:?}, max {:?}, head {:?}) ({} known)",
                min_block_needed, max_block_needed, head_block_num, num_conns
            );
        } else if head_block_num > needed.copied() {
            // we have synced past the needed block
            error!(
                "No archive servers synced (min {:?}, max {:?}, head {:?}) ({} known)",
                min_block_needed, max_block_needed, head_block_num, num_conns
            );
        } else if num_skipped == 0 {
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
        Ok(JsonRpcForwardedResponse::from_str(
            "Requested data is not available",
            Some(-32043),
            Some(request.id),
        ))
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
    ) -> Web3ProxyResult<JsonRpcForwardedResponse> {
        let mut watch_consensus_rpcs = self.watch_consensus_rpcs_sender.subscribe();

        loop {
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
                    // TODO: benchmark this compared to waiting on unbounded futures
                    // TODO: do something with this handle?
                    // TODO: this is not working right. simplify

                    if let Some(request_metadata) = request_metadata {
                        let mut backup_used = false;

                        request_metadata.backend_requests.lock().extend(
                            active_request_handles.iter().map(|x| {
                                let rpc = x.clone_connection();

                                if rpc.backup {
                                    // TODO: its possible we serve from a synced connection though. think about this more
                                    backup_used = true;
                                }

                                x.clone_connection()
                            }),
                        );

                        request_metadata
                            .response_from_backup_rpc
                            .store(true, Ordering::Release);
                    }

                    return self
                        .try_send_parallel_requests(
                            active_request_handles,
                            request.id.clone(),
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
                        request_metadata.no_servers.fetch_add(1, Ordering::Release);
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
                        request_metadata.no_servers.fetch_add(1, Ordering::Release);
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
    }

    pub async fn try_proxy_connection(
        &self,
        authorization: &Arc<Authorization>,
        request: JsonRpcRequest,
        request_metadata: Option<&Arc<RequestMetadata>>,
        min_block_needed: Option<&U64>,
        max_block_needed: Option<&U64>,
    ) -> Web3ProxyResult<JsonRpcForwardedResponse> {
        match authorization.checks.proxy_mode {
            ProxyMode::Debug | ProxyMode::Best => {
                self.try_send_best_consensus_head_connection(
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
        let mut state = serializer.serialize_struct("Web3Rpcs", 6)?;

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

        self.blocks_by_hash.sync();
        self.blocks_by_number.sync();
        state.serialize_field("block_hashes_count", &self.blocks_by_hash.entry_count())?;
        state.serialize_field("block_hashes_size", &self.blocks_by_hash.weighted_size())?;
        state.serialize_field("block_numbers_count", &self.blocks_by_number.entry_count())?;
        state.serialize_field("block_numbers_size", &self.blocks_by_number.weighted_size())?;
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

    // TODO: use request latency instead of head latency
    // TODO: have the latency decay automatically
    let head_ewma = x.head_latency.read().value();

    let active_requests = x.active_requests.load(atomic::Ordering::Relaxed) as f64;

    // TODO: i'm not sure head * active is exactly right. but we'll see
    // TODO: i don't think this actually counts as peak. investigate with atomics.rs and peak_ewma.rs
    let peak_ewma = OrderedFloat(head_ewma * active_requests);

    let backup = x.backup;

    (Reverse(head_block), tier, backup, peak_ewma)
}

mod tests {
    // TODO: why is this allow needed? does tokio::test get in the way somehow?
    #![allow(unused_imports)]

    use super::*;
    use crate::rpcs::consensus::ConsensusFinder;
    use crate::rpcs::{blockchain::Web3ProxyBlock, provider::Web3Provider};
    use arc_swap::ArcSwap;
    use ethers::types::{Block, U256};
    use log::{trace, LevelFilter};
    use parking_lot::RwLock;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::RwLock as AsyncRwLock;

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
                ..Default::default()
            },
            Web3Rpc {
                name: "b".to_string(),
                tier: 0,
                head_block: Some(tx_b),
                ..Default::default()
            },
            Web3Rpc {
                name: "c".to_string(),
                tier: 0,
                head_block: Some(tx_c),
                ..Default::default()
            },
            Web3Rpc {
                name: "d".to_string(),
                tier: 1,
                head_block: Some(tx_d),
                ..Default::default()
            },
            Web3Rpc {
                name: "e".to_string(),
                tier: 1,
                head_block: Some(tx_e),
                ..Default::default()
            },
            Web3Rpc {
                name: "f".to_string(),
                tier: 1,
                head_block: Some(tx_f),
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

        let mut lagged_block: Web3ProxyBlock = lagged_block.try_into().unwrap();
        let mut head_block: Web3ProxyBlock = head_block.try_into().unwrap();

        let block_data_limit = u64::MAX;

        let (tx_synced, _) = watch::channel(Some(head_block.clone()));

        let head_rpc = Web3Rpc {
            name: "synced".to_string(),
            soft_limit: 1_000,
            automatic_block_limit: false,
            backup: false,
            block_data_limit: block_data_limit.into(),
            tier: 0,
            head_block: Some(tx_synced),
            provider: AsyncRwLock::new(Some(Arc::new(Web3Provider::Mock))),
            ..Default::default()
        };

        let (tx_lagged, _) = watch::channel(Some(lagged_block.clone()));

        let lagged_rpc = Web3Rpc {
            name: "lagged".to_string(),
            soft_limit: 1_000,
            automatic_block_limit: false,
            backup: false,
            block_data_limit: block_data_limit.into(),
            tier: 0,
            head_block: Some(tx_lagged),
            provider: AsyncRwLock::new(Some(Arc::new(Web3Provider::Mock))),
            ..Default::default()
        };

        assert!(head_rpc.has_block_data(lagged_block.number()));
        assert!(head_rpc.has_block_data(head_block.number()));

        assert!(lagged_rpc.has_block_data(lagged_block.number()));
        assert!(!lagged_rpc.has_block_data(head_block.number()));

        let head_rpc = Arc::new(head_rpc);
        let lagged_rpc = Arc::new(lagged_rpc);

        let rpcs_by_name = HashMap::from([
            (head_rpc.name.clone(), head_rpc.clone()),
            (lagged_rpc.name.clone(), lagged_rpc.clone()),
        ]);

        let (block_sender, _block_receiver) = flume::unbounded();
        let (pending_tx_id_sender, pending_tx_id_receiver) = flume::unbounded();
        let (watch_consensus_rpcs_sender, _watch_consensus_rpcs_receiver) =
            watch::channel(Default::default());
        let (watch_consensus_head_sender, _watch_consensus_head_receiver) =
            watch::channel(Default::default());

        // TODO: make a Web3Rpcs::new
        let rpcs = Web3Rpcs {
            block_sender,
            by_name: ArcSwap::from_pointee(rpcs_by_name),
            http_interval_sender: None,
            watch_consensus_head_sender: Some(watch_consensus_head_sender),
            watch_consensus_rpcs_sender,
            pending_transaction_cache: Cache::builder()
                .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default()),
            pending_tx_id_receiver,
            pending_tx_id_sender,
            blocks_by_hash: Cache::builder()
                .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default()),
            blocks_by_number: Cache::builder()
                .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default()),
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

        // best_synced_backend_connection requires servers to be synced with the head block
        let x = rpcs
            .best_available_rpc(&authorization, None, &[], None, None)
            .await
            .unwrap();

        dbg!(&x);

        assert!(matches!(x, OpenRequestResult::NotReady));

        // add lagged blocks to the rpcs. both servers should be allowed
        lagged_block = rpcs.try_cache_block(lagged_block, true).await.unwrap();

        rpcs.process_block_from_rpc(
            &authorization,
            &mut consensus_finder,
            Some(lagged_block.clone()),
            lagged_rpc,
            &None,
        )
        .await
        .unwrap();
        rpcs.process_block_from_rpc(
            &authorization,
            &mut consensus_finder,
            Some(lagged_block.clone()),
            head_rpc.clone(),
            &None,
        )
        .await
        .unwrap();

        assert_eq!(rpcs.num_synced_rpcs(), 2);

        // add head block to the rpcs. lagged_rpc should not be available
        head_block = rpcs.try_cache_block(head_block, true).await.unwrap();

        rpcs.process_block_from_rpc(
            &authorization,
            &mut consensus_finder,
            Some(head_block.clone()),
            head_rpc,
            &None,
        )
        .await
        .unwrap();

        assert_eq!(rpcs.num_synced_rpcs(), 1);

        assert!(matches!(
            rpcs.best_available_rpc(&authorization, None, &[], None, None)
                .await,
            Ok(OpenRequestResult::Handle(_))
        ));

        assert!(matches!(
            rpcs.best_available_rpc(&authorization, None, &[], Some(&0.into()), None)
                .await,
            Ok(OpenRequestResult::Handle(_))
        ));

        assert!(matches!(
            rpcs.best_available_rpc(&authorization, None, &[], Some(&1.into()), None)
                .await,
            Ok(OpenRequestResult::Handle(_))
        ));

        // future block should not get a handle
        let future_rpc = rpcs
            .best_available_rpc(&authorization, None, &[], Some(&2.into()), None)
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
            provider: AsyncRwLock::new(Some(Arc::new(Web3Provider::Mock))),
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
            provider: AsyncRwLock::new(Some(Arc::new(Web3Provider::Mock))),
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

        // TODO: make a Web3Rpcs::new
        let rpcs = Web3Rpcs {
            block_sender,
            by_name: ArcSwap::from_pointee(rpcs_by_name),
            http_interval_sender: None,
            watch_consensus_head_sender: Some(watch_consensus_head_sender),
            watch_consensus_rpcs_sender,
            pending_transaction_cache: Cache::builder()
                .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default()),
            pending_tx_id_receiver,
            pending_tx_id_sender,
            blocks_by_hash: Cache::builder()
                .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default()),
            blocks_by_number: Cache::builder()
                .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default()),
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
            .best_available_rpc(&authorization, None, &[], Some(head_block.number()), None)
            .await;

        debug!("best_available_server: {:#?}", best_available_server);

        assert!(matches!(
            best_available_server.unwrap(),
            OpenRequestResult::Handle(_)
        ));

        let _best_available_server_from_none = rpcs
            .best_available_rpc(&authorization, None, &[], None, None)
            .await;

        // assert_eq!(best_available_server, best_available_server_from_none);

        let best_archive_server = rpcs
            .best_available_rpc(&authorization, None, &[], Some(&1.into()), None)
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
            provider: AsyncRwLock::new(Some(Arc::new(Web3Provider::Mock))),
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
            provider: AsyncRwLock::new(Some(Arc::new(Web3Provider::Mock))),
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
            http_interval_sender: None,
            watch_consensus_head_sender: Some(watch_consensus_head_sender),
            watch_consensus_rpcs_sender,
            pending_transaction_cache: Cache::builder()
                .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default()),
            pending_tx_id_receiver,
            pending_tx_id_sender,
            blocks_by_hash: Cache::builder()
                .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default()),
            blocks_by_number: Cache::builder()
                .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default()),
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

/// returns `true` when the desired block number is available
/// TODO: max wait time? max number of blocks to wait for? time is probably best
async fn watch_for_block(
    block_num: Option<&U64>,
    watch_consensus_connections: &mut watch::Receiver<Option<Arc<ConsensusWeb3Rpcs>>>,
) -> Web3ProxyResult<bool> {
    let mut head_block_num = watch_consensus_connections
        .borrow_and_update()
        .as_ref()
        .map(|x| *x.head_block.number());

    match (block_num, head_block_num) {
        (Some(x), Some(ref head)) => {
            if x <= head {
                // we are past this block and no servers have this block
                // this happens if the block is old and all archive servers are offline
                // there is no chance we will get this block without adding an archive server to the config
                return Ok(false);
            }
        }
        (None, None) => {
            // i don't think this is possible
            // maybe if we internally make a request for the latest block and all our servers are disconnected?
            warn!("how'd this None/None happen?");
            return Ok(false);
        }
        (Some(_), None) => {
            // block requested but no servers synced. we will wait
        }
        (None, Some(head)) => {
            // i don't think this is possible
            // maybe if we internally make a request for the latest block and all our servers are disconnected?
            warn!("how'd this None/Some({}) happen?", head);
            return Ok(false);
        }
    };

    // future block is requested
    // wait for the block to arrive
    while head_block_num < block_num.copied() {
        watch_consensus_connections.changed().await?;

        head_block_num = watch_consensus_connections
            .borrow_and_update()
            .as_ref()
            .map(|x| *x.head_block.number());
    }

    Ok(true)
}
