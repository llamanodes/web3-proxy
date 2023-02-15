///! Load balanced communication with a group of web3 rpc providers
use super::blockchain::{BlockHashesCache, Web3ProxyBlock};
use super::consensus::ConsensusWeb3Rpcs;
use super::one::Web3Rpc;
use super::request::{OpenRequestHandle, OpenRequestResult, RequestRevertHandler};
use crate::app::{flatten_handle, AnyhowJoinHandle};
use crate::config::{BlockAndRpc, TxHashAndRpc, Web3RpcConfig};
use crate::frontend::authorization::{Authorization, RequestMetadata};
use crate::frontend::rpc_proxy_ws::ProxyMode;
use crate::jsonrpc::{JsonRpcForwardedResponse, JsonRpcRequest};
use crate::rpcs::transactions::TxStatus;
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
use std::cmp::min_by_key;
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::{cmp, fmt};
use thread_fast_rng::rand::seq::SliceRandom;
use tokio::sync::{broadcast, watch};
use tokio::task;
use tokio::time::{interval, sleep, sleep_until, Duration, Instant, MissedTickBehavior};

/// A collection of web3 connections. Sends requests either the current best server or all servers.
#[derive(From)]
pub struct Web3Rpcs {
    /// any requests will be forwarded to one (or more) of these connections
    pub(crate) by_name: HashMap<String, Arc<Web3Rpc>>,
    /// all providers with the same consensus head block. won't update if there is no `self.watch_consensus_head_sender`
    pub(super) watch_consensus_rpcs_sender: watch::Sender<Arc<ConsensusWeb3Rpcs>>,
    /// this head receiver makes it easy to wait until there is a new block
    pub(super) watch_consensus_head_receiver: Option<watch::Receiver<Option<Web3ProxyBlock>>>,
    pub(super) pending_transactions:
        Cache<TxHash, TxStatus, hashbrown::hash_map::DefaultHashBuilder>,
    /// TODO: this map is going to grow forever unless we do some sort of pruning. maybe store pruned in redis?
    /// all blocks, including orphans
    pub(super) block_hashes: BlockHashesCache,
    /// blocks on the heaviest chain
    pub(super) block_numbers: Cache<U64, H256, hashbrown::hash_map::DefaultHashBuilder>,
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
        block_map: BlockHashesCache,
        chain_id: u64,
        db_conn: Option<DatabaseConnection>,
        http_client: Option<reqwest::Client>,
        max_block_age: Option<u64>,
        max_block_lag: Option<U64>,
        min_head_rpcs: usize,
        min_sum_soft_limit: u32,
        pending_transactions: Cache<TxHash, TxStatus, hashbrown::hash_map::DefaultHashBuilder>,
        pending_tx_sender: Option<broadcast::Sender<TxStatus>>,
        redis_pool: Option<redis_rate_limiter::RedisPool>,
        server_configs: HashMap<String, Web3RpcConfig>,
        watch_consensus_head_sender: Option<watch::Sender<Option<Web3ProxyBlock>>>,
    ) -> anyhow::Result<(Arc<Self>, AnyhowJoinHandle<()>)> {
        let (pending_tx_id_sender, pending_tx_id_receiver) = flume::unbounded();
        let (block_sender, block_receiver) = flume::unbounded::<BlockAndRpc>();

        // TODO: query the rpc to get the actual expected block time, or get from config?
        let expected_block_time_ms = match chain_id {
            // ethereum
            1 => 12_000,
            // polygon
            137 => 2_000,
            // fantom
            250 => 1_000,
            // arbitrum
            42161 => 500,
            // anything else
            _ => {
                warn!("unexpected chain_id. polling every {} seconds", 10);
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

                        if let Err(_) = sender.send(()) {
                            // errors are okay. they mean that all receivers have been dropped, or the rpcs just haven't started yet
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

        // turn configs into connections (in parallel)
        // TODO: move this into a helper function. then we can use it when configs change (will need a remove function too)
        let mut spawn_handles: FuturesUnordered<_> = server_configs
            .into_iter()
            .filter_map(|(server_name, server_config)| {
                if server_config.disabled {
                    info!("{} is disabled", server_name);
                    return None;
                }

                let db_conn = db_conn.clone();
                let http_client = http_client.clone();
                let redis_pool = redis_pool.clone();
                let http_interval_sender = http_interval_sender.clone();

                let block_sender = if watch_consensus_head_sender.is_some() {
                    Some(block_sender.clone())
                } else {
                    None
                };

                let pending_tx_id_sender = Some(pending_tx_id_sender.clone());
                let block_map = block_map.clone();

                debug!("spawning {}", server_name);

                let handle = tokio::spawn(async move {
                    server_config
                        .spawn(
                            server_name,
                            db_conn,
                            redis_pool,
                            chain_id,
                            http_client,
                            http_interval_sender,
                            block_map,
                            block_sender,
                            pending_tx_id_sender,
                            true,
                        )
                        .await
                });

                Some(handle)
            })
            .collect();

        // map of connection names to their connection
        let mut connections = HashMap::new();
        let mut handles = vec![];

        // TODO: futures unordered?
        while let Some(x) = spawn_handles.next().await {
            match x {
                Ok(Ok((connection, handle))) => {
                    // web3 connection worked
                    connections.insert(connection.name.clone(), connection);
                    handles.push(handle);
                }
                Ok(Err(err)) => {
                    // if we got an error here, the app can continue on
                    // TODO: include context about which connection failed
                    error!("Unable to create connection. err={:?}", err);
                }
                Err(err) => {
                    // something actually bad happened. exit with an error
                    return Err(err.into());
                }
            }
        }

        // TODO: max_capacity and time_to_idle from config
        // all block hashes are the same size, so no need for weigher
        let block_hashes = Cache::builder()
            .time_to_idle(Duration::from_secs(600))
            .max_capacity(10_000)
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default());
        // all block numbers are the same size, so no need for weigher
        let block_numbers = Cache::builder()
            .time_to_idle(Duration::from_secs(600))
            .max_capacity(10_000)
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default());

        let (watch_consensus_connections_sender, _) = watch::channel(Default::default());

        let watch_consensus_head_receiver =
            watch_consensus_head_sender.as_ref().map(|x| x.subscribe());

        let connections = Arc::new(Self {
            by_name: connections,
            watch_consensus_rpcs_sender: watch_consensus_connections_sender,
            watch_consensus_head_receiver,
            pending_transactions,
            block_hashes,
            block_numbers,
            min_sum_soft_limit,
            min_head_rpcs,
            max_block_age,
            max_block_lag,
        });

        let authorization = Arc::new(Authorization::internal(db_conn.clone())?);

        let handle = {
            let connections = connections.clone();

            tokio::spawn(async move {
                connections
                    .subscribe(
                        authorization,
                        pending_tx_id_receiver,
                        block_receiver,
                        watch_consensus_head_sender,
                        pending_tx_sender,
                    )
                    .await
            })
        };

        Ok((connections, handle))
    }

    pub fn get(&self, conn_name: &str) -> Option<&Arc<Web3Rpc>> {
        self.by_name.get(conn_name)
    }

    /// subscribe to blocks and transactions from all the backend rpcs.
    /// blocks are processed by all the `Web3Rpc`s and then sent to the `block_receiver`
    /// transaction ids from all the `Web3Rpc`s are deduplicated and forwarded to `pending_tx_sender`
    async fn subscribe(
        self: Arc<Self>,
        authorization: Arc<Authorization>,
        pending_tx_id_receiver: flume::Receiver<TxHashAndRpc>,
        block_receiver: flume::Receiver<BlockAndRpc>,
        head_block_sender: Option<watch::Sender<Option<Web3ProxyBlock>>>,
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
            let handle = task::spawn(async move {
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
        if let Some(head_block_sender) = head_block_sender {
            let connections = Arc::clone(&self);
            let pending_tx_sender = pending_tx_sender.clone();

            let handle = task::Builder::default()
                .name("process_incoming_blocks")
                .spawn(async move {
                    connections
                        .process_incoming_blocks(
                            &authorization,
                            block_receiver,
                            head_block_sender,
                            pending_tx_sender,
                        )
                        .await
                })?;

            futures.push(flatten_handle(handle));
        }

        if futures.is_empty() {
            // no transaction or block subscriptions.

            let handle = task::Builder::default().name("noop").spawn(async move {
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
    ) -> anyhow::Result<JsonRpcForwardedResponse> {
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

    pub async fn best_consensus_head_connection(
        &self,
        authorization: &Arc<Authorization>,
        request_metadata: Option<&Arc<RequestMetadata>>,
        skip: &[Arc<Web3Rpc>],
        // TODO: if we are checking for the consensus head, i don' think we need min_block_needed/max_block_needed
        min_block_needed: Option<&U64>,
        max_block_needed: Option<&U64>,
    ) -> anyhow::Result<OpenRequestResult> {
        let usable_rpcs_by_tier_and_head_number: BTreeMap<(u64, Option<U64>), Vec<Arc<Web3Rpc>>> = {
            let synced_connections = self.watch_consensus_rpcs_sender.borrow().clone();

            let (head_block_num, head_block_age) =
                if let Some(head_block) = synced_connections.head_block.as_ref() {
                    (head_block.number(), head_block.age())
                } else {
                    // TODO: optionally wait for a head_block.number() >= min_block_needed
                    // TODO: though i think that wait would actually need to be earlier in the request
                    return Ok(OpenRequestResult::NotReady);
                };

            let needed_blocks_comparison = match (min_block_needed, max_block_needed) {
                (None, None) => {
                    // no required block given. treat this like the requested the consensus head block
                    cmp::Ordering::Equal
                }
                (None, Some(max_block_needed)) => max_block_needed.cmp(head_block_num),
                (Some(min_block_needed), None) => min_block_needed.cmp(head_block_num),
                (Some(min_block_needed), Some(max_block_needed)) => {
                    match min_block_needed.cmp(max_block_needed) {
                        cmp::Ordering::Equal => min_block_needed.cmp(head_block_num),
                        cmp::Ordering::Greater => {
                            return Err(anyhow::anyhow!(
                                "Invalid blocks bounds requested. min ({}) > max ({})",
                                min_block_needed,
                                max_block_needed
                            ))
                        }
                        cmp::Ordering::Less => {
                            // hmmmm
                            todo!("now what do we do?");
                        }
                    }
                }
            };

            // collect "usable_rpcs_by_head_num_and_weight"
            // TODO: MAKE SURE None SORTS LAST?
            let mut m = BTreeMap::new();

            match needed_blocks_comparison {
                cmp::Ordering::Less => {
                    // need an old block. check all the rpcs. ignore rpcs that are still syncing

                    let min_block_age =
                        self.max_block_age.map(|x| head_block_age.saturating_sub(x));
                    let min_sync_num = self.max_block_lag.map(|x| head_block_num.saturating_sub(x));

                    // TODO: cache this somehow?
                    // TODO: maybe have a helper on synced_connections? that way sum_soft_limits/min_synced_rpcs will be DRY
                    for x in self
                        .by_name
                        .values()
                        .filter(|x| {
                            // TODO: move a bunch of this onto a rpc.is_synced function
                            if skip.contains(x) {
                                // we've already tried this server or have some other reason to skip it
                                false
                            } else if max_block_needed
                                .and_then(|max_block_needed| {
                                    Some(!x.has_block_data(max_block_needed))
                                })
                                .unwrap_or(false)
                            {
                                // server does not have the max block
                                false
                            } else if min_block_needed
                                .and_then(|min_block_needed| {
                                    Some(!x.has_block_data(min_block_needed))
                                })
                                .unwrap_or(false)
                            {
                                // server does not have the min block
                                false
                            } else {
                                // server has the block we need!
                                true
                            }
                        })
                        .cloned()
                    {
                        let x_head_block = x.head_block.read().clone();

                        if let Some(x_head) = x_head_block {
                            // TODO: should nodes that are ahead of the consensus block have priority? seems better to spread the load
                            let x_head_num = x_head.number().min(head_block_num);

                            // TODO: do we really need to check head_num and age?
                            if let Some(min_sync_num) = min_sync_num.as_ref() {
                                if x_head_num < min_sync_num {
                                    continue;
                                }
                            }
                            if let Some(min_block_age) = min_block_age {
                                if x_head.age() < min_block_age {
                                    // rpc is still syncing
                                    continue;
                                }
                            }

                            let key = (x.tier, Some(*x_head_num));

                            m.entry(key).or_insert_with(Vec::new).push(x);
                        }
                    }

                    // TODO: check min_synced_rpcs and min_sum_soft_limits? or maybe better to just try to serve the request?
                }
                cmp::Ordering::Equal => {
                    // need the consensus head block. filter the synced rpcs
                    for x in synced_connections.rpcs.iter().filter(|x| !skip.contains(x)) {
                        // the key doesn't matter if we are checking synced connections. its already sized to what we need
                        let key = (0, None);

                        m.entry(key).or_insert_with(Vec::new).push(x.clone());
                    }
                }
                cmp::Ordering::Greater => {
                    // TODO? if the blocks is close, maybe we could wait for change on a watch_consensus_connections_receiver().subscribe()
                    return Ok(OpenRequestResult::NotReady);
                }
            }

            m
        };

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
                let best_rpc = min_by_key(rpc_a, rpc_b, |x| {
                    OrderedFloat(x.request_latency.read().ewma.value())
                });

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
    pub async fn all_connections(
        &self,
        authorization: &Arc<Authorization>,
        min_block_needed: Option<&U64>,
        max_block_needed: Option<&U64>,
        max_count: Option<usize>,
        always_include_backups: bool,
    ) -> Result<Vec<OpenRequestHandle>, Option<Instant>> {
        if !always_include_backups {
            if let Ok(without_backups) = self
                ._all_connections(
                    false,
                    authorization,
                    min_block_needed,
                    max_block_needed,
                    max_count,
                )
                .await
            {
                return Ok(without_backups);
            }
        }

        self._all_connections(
            true,
            authorization,
            min_block_needed,
            max_block_needed,
            max_count,
        )
        .await
    }

    async fn _all_connections(
        &self,
        allow_backups: bool,
        authorization: &Arc<Authorization>,
        min_block_needed: Option<&U64>,
        max_block_needed: Option<&U64>,
        max_count: Option<usize>,
    ) -> Result<Vec<OpenRequestHandle>, Option<Instant>> {
        let mut earliest_retry_at = None;
        // TODO: with capacity?
        let mut selected_rpcs = vec![];

        let mut max_count = if let Some(max_count) = max_count {
            max_count
        } else {
            self.by_name.len()
        };

        let mut tried = HashSet::new();

        let mut synced_conns = self.watch_consensus_rpcs_sender.borrow().rpcs.clone();

        // synced connections are all on the same block. sort them by tier with higher soft limits first
        synced_conns.sort_by_cached_key(rpc_sync_status_sort_key);

        // if there aren't enough synced connections, include more connections
        // TODO: only do this sorting if the synced_conns isn't enough
        let mut all_conns: Vec<_> = self.by_name.values().cloned().collect();
        all_conns.sort_by_cached_key(rpc_sync_status_sort_key);

        for connection in itertools::chain(synced_conns, all_conns) {
            if max_count == 0 {
                break;
            }

            if tried.contains(&connection.name) {
                continue;
            }

            tried.insert(connection.name.clone());

            if !allow_backups && connection.backup {
                continue;
            }

            if let Some(block_needed) = min_block_needed {
                if !connection.has_block_data(block_needed) {
                    continue;
                }
            }

            if let Some(block_needed) = max_block_needed {
                if !connection.has_block_data(block_needed) {
                    continue;
                }
            }

            // check rate limits and increment our connection counter
            match connection.try_request_handle(authorization, None).await {
                Ok(OpenRequestResult::RetryAt(retry_at)) => {
                    // this rpc is not available. skip it
                    earliest_retry_at = earliest_retry_at.min(Some(retry_at));
                }
                Ok(OpenRequestResult::Handle(handle)) => {
                    max_count -= 1;
                    selected_rpcs.push(handle)
                }
                Ok(OpenRequestResult::NotReady) => {
                    warn!("no request handle for {}", connection)
                }
                Err(err) => {
                    warn!(
                        "error getting request handle for {}. err={:?}",
                        connection, err
                    )
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
    ) -> anyhow::Result<JsonRpcForwardedResponse> {
        let mut skip_rpcs = vec![];
        let mut method_not_available_response = None;

        let mut watch_consensus_connections = self.watch_consensus_rpcs_sender.subscribe();

        // TODO: maximum retries? right now its the total number of servers
        loop {
            let num_skipped = skip_rpcs.len();

            if num_skipped == self.by_name.len() {
                break;
            }

            match self
                .best_consensus_head_connection(
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
                    skip_rpcs.push(active_request_handle.clone_connection());

                    if let Some(request_metadata) = request_metadata {
                        let rpc = active_request_handle.clone_connection();

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
                            RequestRevertHandler::Save,
                            None,
                        )
                        .await;

                    match JsonRpcForwardedResponse::try_from_response_result(
                        response_result,
                        request.id.clone(),
                    ) {
                        Ok(response) => {
                            if let Some(error) = &response.error.as_ref() {
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

                                        // others look like this
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

                            debug!(
                                "Backend server error on {}! Retrying on another. err={:?}",
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

                    // todo!(
                    //     "check if we are requesting an old block and no archive servers are synced"
                    // );

                    if let Some(min_block_needed) = min_block_needed {
                        let mut theres_a_chance = false;

                        for potential_conn in self.by_name.values() {
                            if skip_rpcs.contains(potential_conn) {
                                continue;
                            }

                            // TODO: should we instead check if has_block_data but with the current head block?
                            if potential_conn.has_block_data(min_block_needed) {
                                trace!("chance for {} on {}", min_block_needed, potential_conn);
                                theres_a_chance = true;
                                break;
                            }

                            skip_rpcs.push(potential_conn.clone());
                        }

                        if !theres_a_chance {
                            debug!("no chance of finding data in block #{}", min_block_needed);
                            break;
                        }
                    }

                    debug!("No servers ready. Waiting up for change in synced servers");

                    watch_consensus_connections.changed().await?;
                    watch_consensus_connections.borrow_and_update();
                }
            }
        }

        if let Some(r) = method_not_available_response {
            // TODO: emit a stat for unsupported methods?
            return Ok(r);
        }

        // TODO: do we need this here, or do we do it somewhere else?
        if let Some(request_metadata) = request_metadata {
            request_metadata
                .error_response
                .store(true, Ordering::Release);
        }

        let num_conns = self.by_name.len();
        let num_skipped = skip_rpcs.len();

        if num_skipped == 0 {
            error!("No servers synced ({} known)", num_conns);

            return Ok(JsonRpcForwardedResponse::from_str(
                "No servers synced",
                Some(-32000),
                Some(request.id),
            ));
        } else {
            // TODO: warn? debug? trace?
            warn!(
                "Requested data was not available on {}/{} servers",
                num_skipped, num_conns
            );

            // TODO: what error code?
            // cloudflare gives {"jsonrpc":"2.0","error":{"code":-32043,"message":"Requested data cannot be older than 128 blocks."},"id":1}
            return Ok(JsonRpcForwardedResponse::from_str(
                "Requested data is not available",
                Some(-32043),
                Some(request.id),
            ));
        }
    }

    /// be sure there is a timeout on this or it might loop forever
    pub async fn try_send_all_synced_connections(
        &self,
        authorization: &Arc<Authorization>,
        request: &JsonRpcRequest,
        request_metadata: Option<Arc<RequestMetadata>>,
        min_block_needed: Option<&U64>,
        max_block_needed: Option<&U64>,
        error_level: Level,
        max_count: Option<usize>,
        always_include_backups: bool,
    ) -> anyhow::Result<JsonRpcForwardedResponse> {
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
                    warn!("No servers in sync on {:?}! Retrying", self);

                    if let Some(request_metadata) = &request_metadata {
                        request_metadata.no_servers.fetch_add(1, Ordering::Release);
                    }

                    // TODO: i don't think this will ever happen
                    // TODO: return a 502? if it does?
                    // return Err(anyhow::anyhow!("no available rpcs!"));
                    // TODO: sleep how long?
                    // TODO: subscribe to something in ConsensusConnections instead
                    sleep(Duration::from_millis(200)).await;

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

                    sleep_until(retry_at).await;

                    continue;
                }
            }
        }
    }

    pub async fn try_proxy_connection(
        &self,
        proxy_mode: ProxyMode,
        authorization: &Arc<Authorization>,
        request: JsonRpcRequest,
        request_metadata: Option<&Arc<RequestMetadata>>,
        min_block_needed: Option<&U64>,
        max_block_needed: Option<&U64>,
    ) -> anyhow::Result<JsonRpcForwardedResponse> {
        match proxy_mode {
            ProxyMode::Best => {
                self.try_send_best_consensus_head_connection(
                    authorization,
                    request,
                    request_metadata,
                    min_block_needed,
                    max_block_needed,
                )
                .await
            }
            ProxyMode::Fastest(x) => todo!("Fastest"),
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

        let rpcs: Vec<&Web3Rpc> = self.by_name.values().map(|x| x.as_ref()).collect();
        state.serialize_field("rpcs", &rpcs)?;

        {
            let consensus_connections = self.watch_consensus_rpcs_sender.borrow().clone();
            // TODO: rename synced_connections to consensus_connections?
            state.serialize_field("synced_connections", &consensus_connections)?;
        }

        self.block_hashes.sync();
        self.block_numbers.sync();
        state.serialize_field("block_hashes_count", &self.block_hashes.entry_count())?;
        state.serialize_field("block_hashes_size", &self.block_hashes.weighted_size())?;
        state.serialize_field("block_numbers_count", &self.block_numbers.entry_count())?;
        state.serialize_field("block_numbers_size", &self.block_numbers.weighted_size())?;
        state.end()
    }
}

/// sort by block number (descending) and tier (ascending)
/// TODO: should this be moved into a `impl Web3Rpc`?
/// TODO: i think we still have sorts scattered around the code that should use this
/// TODO: take AsRef or something like that? We don't need an Arc here
fn rpc_sync_status_sort_key(x: &Arc<Web3Rpc>) -> (U64, u64, OrderedFloat<f64>) {
    let reversed_head_block = U64::MAX
        - x.head_block
            .read()
            .as_ref()
            .map(|x| *x.number())
            .unwrap_or_default();

    let tier = x.tier;

    let request_ewma = OrderedFloat(x.request_latency.read().ewma.value());

    (reversed_head_block, tier, request_ewma)
}

mod tests {
    // TODO: why is this allow needed? does tokio::test get in the way somehow?
    #![allow(unused_imports)]
    use super::*;
    use crate::rpcs::consensus::ConsensusFinder;
    use crate::rpcs::{blockchain::Web3ProxyBlock, provider::Web3Provider};
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

        let mut rpcs: Vec<_> = [
            Web3Rpc {
                name: "a".to_string(),
                tier: 0,
                head_block: RwLock::new(None),
                ..Default::default()
            },
            Web3Rpc {
                name: "b".to_string(),
                tier: 0,
                head_block: RwLock::new(blocks.get(1).cloned()),
                ..Default::default()
            },
            Web3Rpc {
                name: "c".to_string(),
                tier: 0,
                head_block: RwLock::new(blocks.get(2).cloned()),
                ..Default::default()
            },
            Web3Rpc {
                name: "d".to_string(),
                tier: 1,
                head_block: RwLock::new(None),
                ..Default::default()
            },
            Web3Rpc {
                name: "e".to_string(),
                tier: 1,
                head_block: RwLock::new(blocks.get(1).cloned()),
                ..Default::default()
            },
            Web3Rpc {
                name: "f".to_string(),
                tier: 1,
                head_block: RwLock::new(blocks.get(2).cloned()),
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

        let now: U256 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .into();

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

        let head_rpc = Web3Rpc {
            name: "synced".to_string(),
            soft_limit: 1_000,
            automatic_block_limit: false,
            backup: false,
            block_data_limit: block_data_limit.into(),
            tier: 0,
            head_block: RwLock::new(Some(head_block.clone())),
            provider: AsyncRwLock::new(Some(Arc::new(Web3Provider::Mock))),
            ..Default::default()
        };

        let lagged_rpc = Web3Rpc {
            name: "lagged".to_string(),
            soft_limit: 1_000,
            automatic_block_limit: false,
            backup: false,
            block_data_limit: block_data_limit.into(),
            tier: 0,
            head_block: RwLock::new(Some(lagged_block.clone())),
            provider: AsyncRwLock::new(Some(Arc::new(Web3Provider::Mock))),
            ..Default::default()
        };

        assert!(head_rpc.has_block_data(&lagged_block.number()));
        assert!(head_rpc.has_block_data(&head_block.number()));

        assert!(lagged_rpc.has_block_data(&lagged_block.number()));
        assert!(!lagged_rpc.has_block_data(&head_block.number()));

        let head_rpc = Arc::new(head_rpc);
        let lagged_rpc = Arc::new(lagged_rpc);

        let rpcs_by_name = HashMap::from([
            (head_rpc.name.clone(), head_rpc.clone()),
            (lagged_rpc.name.clone(), lagged_rpc.clone()),
        ]);

        let (watch_consensus_rpcs_sender, _) = watch::channel(Default::default());

        // TODO: make a Web3Rpcs::new
        let rpcs = Web3Rpcs {
            by_name: rpcs_by_name,
            watch_consensus_head_receiver: None,
            watch_consensus_rpcs_sender,
            pending_transactions: Cache::builder()
                .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default()),
            block_hashes: Cache::builder()
                .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default()),
            block_numbers: Cache::builder()
                .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default()),
            // TODO: test max_block_age?
            max_block_age: None,
            // TODO: test max_block_lag?
            max_block_lag: None,
            min_head_rpcs: 1,
            min_sum_soft_limit: 1,
        };

        let authorization = Arc::new(Authorization::internal(None).unwrap());

        let (head_block_sender, _head_block_receiver) = watch::channel(Default::default());
        let mut consensus_finder = ConsensusFinder::new(&[0, 1, 2, 3], None, None);

        // process None so that
        rpcs.process_block_from_rpc(
            &authorization,
            &mut consensus_finder,
            None,
            lagged_rpc.clone(),
            &head_block_sender,
            &None,
        )
        .await
        .expect("its lagged, but it should still be seen as consensus if its the first to report");
        rpcs.process_block_from_rpc(
            &authorization,
            &mut consensus_finder,
            None,
            head_rpc.clone(),
            &head_block_sender,
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
            .best_consensus_head_connection(&authorization, None, &[], None, None)
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
            &head_block_sender,
            &None,
        )
        .await
        .unwrap();
        rpcs.process_block_from_rpc(
            &authorization,
            &mut consensus_finder,
            Some(lagged_block.clone()),
            head_rpc.clone(),
            &head_block_sender,
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
            &head_block_sender,
            &None,
        )
        .await
        .unwrap();

        assert_eq!(rpcs.num_synced_rpcs(), 1);

        assert!(matches!(
            rpcs.best_consensus_head_connection(&authorization, None, &[], None, None)
                .await,
            Ok(OpenRequestResult::Handle(_))
        ));

        assert!(matches!(
            rpcs.best_consensus_head_connection(&authorization, None, &[], Some(&0.into()), None)
                .await,
            Ok(OpenRequestResult::Handle(_))
        ));

        assert!(matches!(
            rpcs.best_consensus_head_connection(&authorization, None, &[], Some(&1.into()), None)
                .await,
            Ok(OpenRequestResult::Handle(_))
        ));

        // future block should not get a handle
        assert!(matches!(
            rpcs.best_consensus_head_connection(&authorization, None, &[], Some(&2.into()), None)
                .await,
            Ok(OpenRequestResult::NotReady)
        ));
    }

    #[tokio::test]
    async fn test_server_selection_by_archive() {
        // TODO: do this better. can test_env_logger and tokio test be stacked?
        let _ = env_logger::builder()
            .filter_level(LevelFilter::Error)
            .filter_module("web3_proxy", LevelFilter::Trace)
            .is_test(true)
            .try_init();

        let now: U256 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .into();

        let head_block = Block {
            hash: Some(H256::random()),
            number: Some(1_000_000.into()),
            parent_hash: H256::random(),
            timestamp: now,
            ..Default::default()
        };

        let head_block: Web3ProxyBlock = Arc::new(head_block).try_into().unwrap();

        let pruned_rpc = Web3Rpc {
            name: "pruned".to_string(),
            soft_limit: 3_000,
            automatic_block_limit: false,
            backup: false,
            block_data_limit: 64.into(),
            tier: 1,
            head_block: RwLock::new(Some(head_block.clone())),
            ..Default::default()
        };

        let archive_rpc = Web3Rpc {
            name: "archive".to_string(),
            soft_limit: 1_000,
            automatic_block_limit: false,
            backup: false,
            block_data_limit: u64::MAX.into(),
            tier: 2,
            head_block: RwLock::new(Some(head_block.clone())),
            ..Default::default()
        };

        assert!(pruned_rpc.has_block_data(&head_block.number()));
        assert!(archive_rpc.has_block_data(&head_block.number()));
        assert!(!pruned_rpc.has_block_data(&1.into()));
        assert!(archive_rpc.has_block_data(&1.into()));

        let pruned_rpc = Arc::new(pruned_rpc);
        let archive_rpc = Arc::new(archive_rpc);

        let rpcs_by_name = HashMap::from([
            (pruned_rpc.name.clone(), pruned_rpc.clone()),
            (archive_rpc.name.clone(), archive_rpc.clone()),
        ]);

        let (watch_consensus_rpcs_sender, _) = watch::channel(Default::default());

        // TODO: make a Web3Rpcs::new
        let rpcs = Web3Rpcs {
            by_name: rpcs_by_name,
            watch_consensus_head_receiver: None,
            watch_consensus_rpcs_sender,
            pending_transactions: Cache::builder()
                .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default()),
            block_hashes: Cache::builder()
                .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default()),
            block_numbers: Cache::builder()
                .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default()),
            min_head_rpcs: 1,
            min_sum_soft_limit: 4_000,
            max_block_age: None,
            max_block_lag: None,
        };

        let authorization = Arc::new(Authorization::internal(None).unwrap());

        let (head_block_sender, _head_block_receiver) = watch::channel(Default::default());
        let mut connection_heads = ConsensusFinder::new(&[0, 1, 2, 3], None, None);

        // min sum soft limit will require tier 2
        rpcs.process_block_from_rpc(
            &authorization,
            &mut connection_heads,
            Some(head_block.clone()),
            pruned_rpc.clone(),
            &head_block_sender,
            &None,
        )
        .await
        .unwrap_err();

        rpcs.process_block_from_rpc(
            &authorization,
            &mut connection_heads,
            Some(head_block.clone()),
            archive_rpc.clone(),
            &head_block_sender,
            &None,
        )
        .await
        .unwrap();

        assert_eq!(rpcs.num_synced_rpcs(), 2);

        // best_synced_backend_connection requires servers to be synced with the head block
        // TODO: test with and without passing the head_block.number?
        let best_head_server = rpcs
            .best_consensus_head_connection(
                &authorization,
                None,
                &[],
                Some(&head_block.number()),
                None,
            )
            .await;

        debug!("best_head_server: {:#?}", best_head_server);

        assert!(matches!(
            best_head_server.unwrap(),
            OpenRequestResult::Handle(_)
        ));

        let best_archive_server = rpcs
            .best_consensus_head_connection(&authorization, None, &[], Some(&1.into()), None)
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
}
