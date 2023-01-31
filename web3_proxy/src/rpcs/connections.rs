///! Load balanced communication with a group of web3 providers
use super::blockchain::{ArcBlock, BlockHashesCache};
use super::connection::Web3Connection;
use super::request::{
    OpenRequestHandle, OpenRequestHandleMetrics, OpenRequestResult, RequestRevertHandler,
};
use super::synced_connections::ConsensusConnections;
use crate::app::{flatten_handle, AnyhowJoinHandle};
use crate::config::{BlockAndRpc, TxHashAndRpc, Web3ConnectionConfig};
use crate::frontend::authorization::{Authorization, RequestMetadata};
use crate::frontend::rpc_proxy_ws::ProxyMode;
use crate::jsonrpc::{JsonRpcForwardedResponse, JsonRpcRequest};
use crate::rpcs::transactions::TxStatus;
use counter::Counter;
use derive_more::From;
use ethers::prelude::{ProviderError, TxHash, H256, U64};
use futures::future::{join_all, try_join_all};
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use hashbrown::{HashMap, HashSet};
use log::{debug, error, info, trace, warn, Level};
use migration::sea_orm::DatabaseConnection;
use moka::future::{Cache, ConcurrentCacheExt};
use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;
use serde_json::json;
use serde_json::value::RawValue;
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
pub struct Web3Connections {
    /// any requests will be forwarded to one (or more) of these connections
    pub(crate) conns: HashMap<String, Arc<Web3Connection>>,
    /// all providers with the same consensus head block. won't update if there is no `self.watch_consensus_head_sender`
    pub(super) watch_consensus_connections_sender: watch::Sender<Arc<ConsensusConnections>>,
    /// this head receiver makes it easy to wait until there is a new block
    pub(super) watch_consensus_head_receiver: Option<watch::Receiver<ArcBlock>>,
    pub(super) pending_transactions:
        Cache<TxHash, TxStatus, hashbrown::hash_map::DefaultHashBuilder>,
    /// TODO: this map is going to grow forever unless we do some sort of pruning. maybe store pruned in redis?
    /// all blocks, including orphans
    pub(super) block_hashes: BlockHashesCache,
    /// blocks on the heaviest chain
    pub(super) block_numbers: Cache<U64, H256, hashbrown::hash_map::DefaultHashBuilder>,
    pub(super) min_head_rpcs: usize,
    pub(super) min_sum_soft_limit: u32,
}

impl Web3Connections {
    /// Spawn durable connections to multiple Web3 providers.
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn(
        chain_id: u64,
        db_conn: Option<DatabaseConnection>,
        server_configs: HashMap<String, Web3ConnectionConfig>,
        http_client: Option<reqwest::Client>,
        redis_pool: Option<redis_rate_limiter::RedisPool>,
        block_map: BlockHashesCache,
        watch_consensus_head_sender: Option<watch::Sender<ArcBlock>>,
        min_sum_soft_limit: u32,
        min_head_rpcs: usize,
        pending_tx_sender: Option<broadcast::Sender<TxStatus>>,
        pending_transactions: Cache<TxHash, TxStatus, hashbrown::hash_map::DefaultHashBuilder>,
        open_request_handle_metrics: Arc<OpenRequestHandleMetrics>,
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
            let (sender, receiver) = broadcast::channel(1);

            drop(receiver);

            // TODO: what interval? follow a websocket also? maybe by watching synced connections with a timeout. will need debounce
            let mut interval = interval(Duration::from_millis(expected_block_time_ms));
            interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

            let sender = Arc::new(sender);

            let f = {
                let sender = sender.clone();

                async move {
                    loop {
                        // TODO: every time a head_block arrives (with a small delay for known slow servers), or on the interval.
                        interval.tick().await;

                        // // trace!("http interval ready");

                        // errors are okay. they mean that all receivers have been dropped
                        let _ = sender.send(());
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
        // TODO: futures unordered?
        let spawn_handles: Vec<_> = server_configs
            .into_iter()
            .filter_map(|(server_name, server_config)| {
                if server_config.disabled {
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
                let open_request_handle_metrics = open_request_handle_metrics.clone();

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
                            open_request_handle_metrics,
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
        for x in join_all(spawn_handles).await {
            // TODO: how should we handle errors here? one rpc being down shouldn't cause the program to exit
            match x {
                Ok(Ok((connection, handle))) => {
                    connections.insert(connection.name.clone(), connection);
                    handles.push(handle);
                }
                Ok(Err(err)) => {
                    // if we got an error here, it is not retryable
                    // TODO: include context about which connection failed
                    error!("Unable to create connection. err={:?}", err);
                }
                Err(err) => {
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
            conns: connections,
            watch_consensus_connections_sender,
            watch_consensus_head_receiver,
            pending_transactions,
            block_hashes,
            block_numbers,
            min_sum_soft_limit,
            min_head_rpcs,
        });

        let authorization = Arc::new(Authorization::internal(db_conn.clone())?);

        let handle = {
            let connections = connections.clone();

            tokio::spawn(async move {
                // TODO: try_join_all with the other handles here
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

    pub fn get(&self, conn_name: &str) -> Option<&Arc<Web3Connection>> {
        self.conns.get(conn_name)
    }

    /// subscribe to blocks and transactions from all the backend rpcs.
    /// blocks are processed by all the `Web3Connection`s and then sent to the `block_receiver`
    /// transaction ids from all the `Web3Connection`s are deduplicated and forwarded to `pending_tx_sender`
    async fn subscribe(
        self: Arc<Self>,
        authorization: Arc<Authorization>,
        pending_tx_id_receiver: flume::Receiver<TxHashAndRpc>,
        block_receiver: flume::Receiver<BlockAndRpc>,
        head_block_sender: Option<watch::Sender<ArcBlock>>,
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
        skip: &[Arc<Web3Connection>],
        min_block_needed: Option<&U64>,
    ) -> anyhow::Result<OpenRequestResult> {
        if let Ok(without_backups) = self
            ._best_consensus_head_connection(
                false,
                authorization,
                request_metadata,
                skip,
                min_block_needed,
            )
            .await
        {
            // TODO: this might use backups too eagerly. but even when we allow backups, we still prioritize our own
            if matches!(without_backups, OpenRequestResult::Handle(_)) {
                return Ok(without_backups);
            }
        }

        self._best_consensus_head_connection(
            true,
            authorization,
            request_metadata,
            skip,
            min_block_needed,
        )
        .await
    }

    /// get the best available rpc server with the consensus head block. it might have blocks after the consensus head
    async fn _best_consensus_head_connection(
        &self,
        allow_backups: bool,
        authorization: &Arc<Authorization>,
        request_metadata: Option<&Arc<RequestMetadata>>,
        skip: &[Arc<Web3Connection>],
        min_block_needed: Option<&U64>,
    ) -> anyhow::Result<OpenRequestResult> {
        let usable_rpcs_by_head_num_and_weight: BTreeMap<
            (Option<U64>, u64),
            Vec<Arc<Web3Connection>>,
        > = {
            let synced_connections = self.watch_consensus_connections_sender.borrow().clone();

            let head_block_num = if let Some(head_block) = synced_connections.head_block.as_ref() {
                head_block.number()
            } else {
                // TODO: optionally wait for a head block >= min_block_needed
                return Ok(OpenRequestResult::NotReady(allow_backups));
            };

            let min_block_needed = min_block_needed.unwrap_or(&head_block_num);

            let mut m = BTreeMap::new();

            match min_block_needed.cmp(&head_block_num) {
                cmp::Ordering::Less => {
                    // need an old block. check all the rpcs. prefer the most synced
                    for x in self
                        .conns
                        .values()
                        .filter(|x| if allow_backups { true } else { !x.backup })
                        .filter(|x| !skip.contains(x))
                        .filter(|x| x.has_block_data(min_block_needed))
                        .cloned()
                    {
                        let x_head_block = x.head_block.read().clone();

                        match x_head_block {
                            None => continue,
                            Some(x_head) => {
                                let key = (Some(x_head.number()), u64::MAX - x.tier);

                                m.entry(key).or_insert_with(Vec::new).push(x);
                            }
                        }
                    }
                }
                cmp::Ordering::Equal => {
                    // need the consensus head block. filter the synced rpcs
                    for x in synced_connections
                        .conns
                        .iter()
                        .filter(|x| !skip.contains(x))
                    {
                        let key = (None, u64::MAX - x.tier);

                        m.entry(key).or_insert_with(Vec::new).push(x.clone());
                    }
                }
                cmp::Ordering::Greater => {
                    // TODO? if the blocks is close and wait_for_sync and allow_backups, wait for change on a watch_consensus_connections_receiver().subscribe()
                    return Ok(OpenRequestResult::NotReady(allow_backups));
                }
            }

            m
        };

        let mut earliest_retry_at = None;

        for usable_rpcs in usable_rpcs_by_head_num_and_weight.into_values().rev() {
            // under heavy load, it is possible for even our best server to be negative
            let mut minimum = f64::MAX;
            let mut maximum = f64::MIN;

            // we sort on a combination of values. cache them here so that we don't do this math multiple times.
            let mut available_request_map: HashMap<_, f64> = usable_rpcs
                .iter()
                .map(|rpc| {
                    // TODO: are active requests what we want? do we want a counter for requests in the last second + any actives longer than that?
                    // TODO: get active requests out of redis (that's definitely too slow)
                    // TODO: do something with hard limit instead? (but that is hitting redis too much)
                    let active_requests = rpc.active_requests() as f64;
                    let soft_limit = rpc.soft_limit as f64;

                    let available_requests = soft_limit - active_requests;

                    // trace!("available requests on {}: {}", rpc, available_requests);

                    minimum = minimum.min(available_requests);
                    maximum = maximum.max(available_requests);

                    (rpc, available_requests)
                })
                .collect();

            // trace!("minimum available requests: {}", minimum);
            // trace!("maximum available requests: {}", maximum);

            if maximum < 0.0 {
                // TODO: if maximum < 0 and there are other tiers on the same block, we should include them now
                warn!("soft limits overloaded: {} to {}", minimum, maximum)
            }

            // choose_multiple_weighted can't have negative numbers. shift up if any are negative
            // TODO: is this a correct way to shift?
            if minimum < 0.0 {
                available_request_map = available_request_map
                    .into_iter()
                    .map(|(rpc, available_requests)| {
                        // TODO: is simple addition the right way to shift everyone?
                        // TODO: probably want something non-linear
                        // minimum is negative, so we subtract to make available requests bigger
                        let x = available_requests - minimum;

                        (rpc, x)
                    })
                    .collect()
            }

            let sorted_rpcs = {
                if usable_rpcs.len() == 1 {
                    // TODO: return now instead? we shouldn't need another alloc
                    vec![usable_rpcs.get(0).expect("there should be 1")]
                } else {
                    let mut rng = thread_fast_rng::thread_fast_rng();

                    usable_rpcs
                        .choose_multiple_weighted(&mut rng, usable_rpcs.len(), |rpc| {
                            *available_request_map
                                .get(rpc)
                                .expect("rpc should always be in available_request_map")
                        })
                        .unwrap()
                        .collect::<Vec<_>>()
                }
            };

            // now that the rpcs are sorted, try to get an active request handle for one of them
            for best_rpc in sorted_rpcs.into_iter() {
                // increment our connection counter
                match best_rpc
                    .try_request_handle(authorization, min_block_needed.is_none())
                    .await
                {
                    Ok(OpenRequestResult::Handle(handle)) => {
                        // trace!("opened handle: {}", best_rpc);
                        return Ok(OpenRequestResult::Handle(handle));
                    }
                    Ok(OpenRequestResult::RetryAt(retry_at)) => {
                        earliest_retry_at = earliest_retry_at.min(Some(retry_at));
                    }
                    Ok(OpenRequestResult::NotReady(_)) => {
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

                Ok(OpenRequestResult::NotReady(allow_backups))
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
        block_needed: Option<&U64>,
        max_count: Option<usize>,
    ) -> Result<Vec<OpenRequestHandle>, Option<Instant>> {
        if let Ok(without_backups) = self
            ._all_connections(false, authorization, block_needed, max_count)
            .await
        {
            return Ok(without_backups);
        }

        self._all_connections(true, authorization, block_needed, max_count)
            .await
    }

    async fn _all_connections(
        &self,
        allow_backups: bool,
        authorization: &Arc<Authorization>,
        block_needed: Option<&U64>,
        max_count: Option<usize>,
    ) -> Result<Vec<OpenRequestHandle>, Option<Instant>> {
        let mut earliest_retry_at = None;
        // TODO: with capacity?
        let mut selected_rpcs = vec![];

        let mut max_count = if let Some(max_count) = max_count {
            max_count
        } else {
            self.conns.len()
        };

        let mut tried = HashSet::new();

        let conns_to_try = itertools::chain(
            // TODO: sort by tier
            self.watch_consensus_connections_sender
                .borrow()
                .conns
                .clone(),
            // TODO: sort by tier
            self.conns.values().cloned(),
        );

        for connection in conns_to_try {
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

            if let Some(block_needed) = block_needed {
                if !connection.has_block_data(block_needed) {
                    continue;
                }
            }

            // check rate limits and increment our connection counter
            match connection
                .try_request_handle(authorization, block_needed.is_none())
                .await
            {
                Ok(OpenRequestResult::RetryAt(retry_at)) => {
                    // this rpc is not available. skip it
                    earliest_retry_at = earliest_retry_at.min(Some(retry_at));
                }
                Ok(OpenRequestResult::Handle(handle)) => {
                    max_count -= 1;
                    selected_rpcs.push(handle)
                }
                Ok(OpenRequestResult::NotReady(_)) => {
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
    ) -> anyhow::Result<JsonRpcForwardedResponse> {
        let mut skip_rpcs = vec![];
        let mut method_not_available_response = None;

        let mut watch_consensus_connections = self.watch_consensus_connections_sender.subscribe();

        // TODO: maximum retries? right now its the total number of servers
        loop {
            let num_skipped = skip_rpcs.len();

            if num_skipped == self.conns.len() {
                break;
            }

            match self
                .best_consensus_head_connection(
                    authorization,
                    request_metadata,
                    &skip_rpcs,
                    min_block_needed,
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
                OpenRequestResult::NotReady(backups_included) => {
                    if let Some(request_metadata) = request_metadata {
                        request_metadata.no_servers.fetch_add(1, Ordering::Release);
                    }

                    // todo!(
                    //     "check if we are requesting an old block and no archive servers are synced"
                    // );

                    if let Some(min_block_needed) = min_block_needed {
                        let mut theres_a_chance = false;

                        for potential_conn in self.conns.values() {
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

                    if backups_included {
                        // if NotReady and we tried backups, there's no chance
                        warn!("No servers ready even after checking backups");
                        break;
                    }

                    debug!("No servers ready. Waiting up to 1 second for change in synced servers");

                    // TODO: exponential backoff?
                    tokio::select! {
                        _ = sleep(Duration::from_secs(1)) => {
                            // do NOT pop the last rpc off skip here
                        }
                        _ = watch_consensus_connections.changed() => {
                            watch_consensus_connections.borrow_and_update();
                        }
                    }
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

        let num_conns = self.conns.len();
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
        block_needed: Option<&U64>,
        error_level: Level,
        max_count: Option<usize>,
    ) -> anyhow::Result<JsonRpcForwardedResponse> {
        loop {
            match self
                .all_connections(authorization, block_needed, max_count)
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
    ) -> anyhow::Result<JsonRpcForwardedResponse> {
        match proxy_mode {
            ProxyMode::Best => {
                self.try_send_best_consensus_head_connection(
                    authorization,
                    request,
                    request_metadata,
                    min_block_needed,
                )
                .await
            }
            ProxyMode::Fastest(x) => todo!("Fastest"),
            ProxyMode::Versus => todo!("Versus"),
        }
    }
}

impl fmt::Debug for Web3Connections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("Web3Connections")
            .field("conns", &self.conns)
            .finish_non_exhaustive()
    }
}

impl Serialize for Web3Connections {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("Web3Connections", 6)?;

        let conns: Vec<&Web3Connection> = self.conns.values().map(|x| x.as_ref()).collect();
        state.serialize_field("conns", &conns)?;

        {
            let consensus_connections = self.watch_consensus_connections_sender.borrow().clone();
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

mod tests {
    // TODO: why is this allow needed? does tokio::test get in the way somehow?
    #![allow(unused_imports)]
    use super::*;
    use crate::rpcs::{
        blockchain::{ConsensusFinder, SavedBlock},
        connection::ProviderState,
        provider::Web3Provider,
    };
    use ethers::types::{Block, U256};
    use log::{trace, LevelFilter};
    use parking_lot::RwLock;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::RwLock as AsyncRwLock;

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

        // TODO: write a impl From for Block -> BlockId?
        let mut lagged_block: SavedBlock = lagged_block.into();
        let mut head_block: SavedBlock = head_block.into();

        let block_data_limit = u64::MAX;

        let head_rpc = Web3Connection {
            name: "synced".to_string(),
            db_conn: None,
            display_name: None,
            url: "ws://example.com/synced".to_string(),
            http_client: None,
            active_requests: 0.into(),
            frontend_requests: 0.into(),
            internal_requests: 0.into(),
            provider_state: AsyncRwLock::new(ProviderState::Connected(Arc::new(
                Web3Provider::Mock,
            ))),
            hard_limit: None,
            hard_limit_until: None,
            soft_limit: 1_000,
            automatic_block_limit: true,
            backup: false,
            block_data_limit: block_data_limit.into(),
            tier: 0,
            head_block: RwLock::new(Some(head_block.clone())),
            open_request_handle_metrics: Arc::new(Default::default()),
        };

        let lagged_rpc = Web3Connection {
            name: "lagged".to_string(),
            db_conn: None,
            display_name: None,
            url: "ws://example.com/lagged".to_string(),
            http_client: None,
            active_requests: 0.into(),
            frontend_requests: 0.into(),
            internal_requests: 0.into(),
            provider_state: AsyncRwLock::new(ProviderState::Connected(Arc::new(
                Web3Provider::Mock,
            ))),
            hard_limit: None,
            hard_limit_until: None,
            soft_limit: 1_000,
            automatic_block_limit: false,
            backup: false,
            block_data_limit: block_data_limit.into(),
            tier: 0,
            head_block: RwLock::new(Some(lagged_block.clone())),
            open_request_handle_metrics: Arc::new(Default::default()),
        };

        assert!(head_rpc.has_block_data(&lagged_block.number()));
        assert!(head_rpc.has_block_data(&head_block.number()));

        assert!(lagged_rpc.has_block_data(&lagged_block.number()));
        assert!(!lagged_rpc.has_block_data(&head_block.number()));

        let head_rpc = Arc::new(head_rpc);
        let lagged_rpc = Arc::new(lagged_rpc);

        let conns = HashMap::from([
            (head_rpc.name.clone(), head_rpc.clone()),
            (lagged_rpc.name.clone(), lagged_rpc.clone()),
        ]);

        let (watch_consensus_connections_sender, _) = watch::channel(Default::default());

        // TODO: make a Web3Connections::new
        let conns = Web3Connections {
            conns,
            watch_consensus_head_receiver: None,
            watch_consensus_connections_sender,
            pending_transactions: Cache::builder()
                .max_capacity(10_000)
                .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default()),
            block_hashes: Cache::builder()
                .max_capacity(10_000)
                .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default()),
            block_numbers: Cache::builder()
                .max_capacity(10_000)
                .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default()),
            min_head_rpcs: 1,
            min_sum_soft_limit: 1,
        };

        let authorization = Arc::new(Authorization::internal(None).unwrap());

        let (head_block_sender, _head_block_receiver) =
            watch::channel::<ArcBlock>(Default::default());
        let mut connection_heads = ConsensusFinder::default();

        // process None so that
        conns
            .process_block_from_rpc(
                &authorization,
                &mut connection_heads,
                None,
                lagged_rpc.clone(),
                &head_block_sender,
                &None,
            )
            .await
            .unwrap();
        conns
            .process_block_from_rpc(
                &authorization,
                &mut connection_heads,
                None,
                head_rpc.clone(),
                &head_block_sender,
                &None,
            )
            .await
            .unwrap();

        // no head block because the rpcs haven't communicated through their channels
        assert!(conns.head_block_hash().is_none());

        // all_backend_connections gives everything regardless of sync status
        assert_eq!(
            conns
                .all_connections(&authorization, None, None)
                .await
                .unwrap()
                .len(),
            2
        );

        // best_synced_backend_connection requires servers to be synced with the head block
        let x = conns
            .best_consensus_head_connection(&authorization, None, &[], None)
            .await
            .unwrap();

        dbg!(&x);

        assert!(matches!(x, OpenRequestResult::NotReady(true)));

        // add lagged blocks to the conns. both servers should be allowed
        lagged_block.block = conns.save_block(lagged_block.block, true).await.unwrap();

        conns
            .process_block_from_rpc(
                &authorization,
                &mut connection_heads,
                Some(lagged_block.clone()),
                lagged_rpc,
                &head_block_sender,
                &None,
            )
            .await
            .unwrap();
        conns
            .process_block_from_rpc(
                &authorization,
                &mut connection_heads,
                Some(lagged_block.clone()),
                head_rpc.clone(),
                &head_block_sender,
                &None,
            )
            .await
            .unwrap();

        assert_eq!(conns.num_synced_rpcs(), 2);

        // add head block to the conns. lagged_rpc should not be available
        head_block.block = conns.save_block(head_block.block, true).await.unwrap();

        conns
            .process_block_from_rpc(
                &authorization,
                &mut connection_heads,
                Some(head_block.clone()),
                head_rpc,
                &head_block_sender,
                &None,
            )
            .await
            .unwrap();

        assert_eq!(conns.num_synced_rpcs(), 1);

        assert!(matches!(
            conns
                .best_consensus_head_connection(&authorization, None, &[], None)
                .await,
            Ok(OpenRequestResult::Handle(_))
        ));

        assert!(matches!(
            conns
                .best_consensus_head_connection(&authorization, None, &[], Some(&0.into()))
                .await,
            Ok(OpenRequestResult::Handle(_))
        ));

        assert!(matches!(
            conns
                .best_consensus_head_connection(&authorization, None, &[], Some(&1.into()))
                .await,
            Ok(OpenRequestResult::Handle(_))
        ));

        // future block should not get a handle
        assert!(matches!(
            conns
                .best_consensus_head_connection(&authorization, None, &[], Some(&2.into()))
                .await,
            Ok(OpenRequestResult::NotReady(true))
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

        let head_block: SavedBlock = Arc::new(head_block).into();

        let pruned_rpc = Web3Connection {
            name: "pruned".to_string(),
            db_conn: None,
            display_name: None,
            url: "ws://example.com/pruned".to_string(),
            http_client: None,
            active_requests: 0.into(),
            frontend_requests: 0.into(),
            internal_requests: 0.into(),
            provider_state: AsyncRwLock::new(ProviderState::Connected(Arc::new(
                Web3Provider::Mock,
            ))),
            hard_limit: None,
            hard_limit_until: None,
            soft_limit: 3_000,
            automatic_block_limit: false,
            backup: false,
            block_data_limit: 64.into(),
            tier: 1,
            head_block: RwLock::new(Some(head_block.clone())),
            open_request_handle_metrics: Arc::new(Default::default()),
        };

        let archive_rpc = Web3Connection {
            name: "archive".to_string(),
            db_conn: None,
            display_name: None,
            url: "ws://example.com/archive".to_string(),
            http_client: None,
            active_requests: 0.into(),
            frontend_requests: 0.into(),
            internal_requests: 0.into(),
            provider_state: AsyncRwLock::new(ProviderState::Connected(Arc::new(
                Web3Provider::Mock,
            ))),
            hard_limit: None,
            hard_limit_until: None,
            soft_limit: 1_000,
            automatic_block_limit: false,
            backup: false,
            block_data_limit: u64::MAX.into(),
            tier: 2,
            head_block: RwLock::new(Some(head_block.clone())),
            open_request_handle_metrics: Arc::new(Default::default()),
        };

        assert!(pruned_rpc.has_block_data(&head_block.number()));
        assert!(archive_rpc.has_block_data(&head_block.number()));
        assert!(!pruned_rpc.has_block_data(&1.into()));
        assert!(archive_rpc.has_block_data(&1.into()));

        let pruned_rpc = Arc::new(pruned_rpc);
        let archive_rpc = Arc::new(archive_rpc);

        let conns = HashMap::from([
            (pruned_rpc.name.clone(), pruned_rpc.clone()),
            (archive_rpc.name.clone(), archive_rpc.clone()),
        ]);

        let (watch_consensus_connections_sender, _) = watch::channel(Default::default());

        // TODO: make a Web3Connections::new
        let conns = Web3Connections {
            conns,
            watch_consensus_head_receiver: None,
            watch_consensus_connections_sender,
            pending_transactions: Cache::builder()
                .max_capacity(10)
                .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default()),
            block_hashes: Cache::builder()
                .max_capacity(10)
                .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default()),
            block_numbers: Cache::builder()
                .max_capacity(10)
                .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default()),
            min_head_rpcs: 1,
            min_sum_soft_limit: 3_000,
        };

        let authorization = Arc::new(Authorization::internal(None).unwrap());

        let (head_block_sender, _head_block_receiver) =
            watch::channel::<ArcBlock>(Default::default());
        let mut connection_heads = ConsensusFinder::default();

        conns
            .process_block_from_rpc(
                &authorization,
                &mut connection_heads,
                Some(head_block.clone()),
                pruned_rpc.clone(),
                &head_block_sender,
                &None,
            )
            .await
            .unwrap();
        conns
            .process_block_from_rpc(
                &authorization,
                &mut connection_heads,
                Some(head_block.clone()),
                archive_rpc.clone(),
                &head_block_sender,
                &None,
            )
            .await
            .unwrap();

        assert_eq!(conns.num_synced_rpcs(), 2);

        // best_synced_backend_connection requires servers to be synced with the head block
        let best_head_server = conns
            .best_consensus_head_connection(&authorization, None, &[], Some(&head_block.number()))
            .await;

        assert!(matches!(
            best_head_server.unwrap(),
            OpenRequestResult::Handle(_)
        ));

        let best_archive_server = conns
            .best_consensus_head_connection(&authorization, None, &[], Some(&1.into()))
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
