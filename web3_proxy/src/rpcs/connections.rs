///! Load balanced communication with a group of web3 providers
use super::blockchain::{ArcBlock, BlockHashesCache};
use super::connection::Web3Connection;
use super::request::{OpenRequestHandle, OpenRequestHandleMetrics, OpenRequestResult};
use super::synced_connections::SyncedConnections;
use crate::app::{flatten_handle, AnyhowJoinHandle};
use crate::config::{BlockAndRpc, TxHashAndRpc, Web3ConnectionConfig};
use crate::jsonrpc::{JsonRpcForwardedResponse, JsonRpcRequest};
use crate::rpcs::transactions::TxStatus;
use arc_swap::ArcSwap;
use counter::Counter;
use derive_more::From;
use ethers::prelude::{ProviderError, TxHash, H256, U64};
use futures::future::{join_all, try_join_all};
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use hashbrown::HashMap;
use moka::future::Cache;
use petgraph::graphmap::DiGraphMap;
use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;
use serde_json::value::RawValue;
use std::cmp;
use std::cmp::Reverse;
use std::fmt;
use std::sync::Arc;
use tokio::sync::RwLock as AsyncRwLock;
use tokio::sync::{broadcast, watch};
use tokio::task;
use tokio::time::{interval, sleep, sleep_until, Duration, Instant, MissedTickBehavior};
use tracing::{error, info, instrument, trace, warn};

/// A collection of web3 connections. Sends requests either the current best server or all servers.
#[derive(From)]
pub struct Web3Connections {
    pub(super) conns: HashMap<String, Arc<Web3Connection>>,
    /// any requests will be forwarded to one (or more) of these connections
    pub(super) synced_connections: ArcSwap<SyncedConnections>,
    pub(super) pending_transactions: Cache<TxHash, TxStatus, ahash::RandomState>,
    /// TODO: this map is going to grow forever unless we do some sort of pruning. maybe store pruned in redis?
    /// all blocks, including orphans
    pub(super) block_hashes: BlockHashesCache,
    /// blocks on the heaviest chain
    pub(super) block_numbers: Cache<U64, H256, ahash::RandomState>,
    /// TODO: this map is going to grow forever unless we do some sort of pruning. maybe store pruned in redis?
    /// TODO: what should we use for edges?
    pub(super) blockchain_graphmap: AsyncRwLock<DiGraphMap<H256, u32>>,
    pub(super) min_synced_rpcs: usize,
    pub(super) min_sum_soft_limit: u32,
}

impl Web3Connections {
    /// Spawn durable connections to multiple Web3 providers.
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn(
        chain_id: u64,
        server_configs: HashMap<String, Web3ConnectionConfig>,
        http_client: Option<reqwest::Client>,
        redis_pool: Option<redis_rate_limiter::RedisPool>,
        block_map: BlockHashesCache,
        head_block_sender: Option<watch::Sender<ArcBlock>>,
        min_sum_soft_limit: u32,
        min_synced_rpcs: usize,
        pending_tx_sender: Option<broadcast::Sender<TxStatus>>,
        pending_transactions: Cache<TxHash, TxStatus, ahash::RandomState>,
        open_request_handle_metrics: Arc<OpenRequestHandleMetrics>,
    ) -> anyhow::Result<(Arc<Self>, AnyhowJoinHandle<()>)> {
        let (pending_tx_id_sender, pending_tx_id_receiver) = flume::unbounded();
        let (block_sender, block_receiver) = flume::unbounded::<BlockAndRpc>();

        let http_interval_sender = if http_client.is_some() {
            let (sender, receiver) = broadcast::channel(1);

            drop(receiver);

            // TODO: what interval? follow a websocket also? maybe by watching synced connections with a timeout. will need debounce
            let mut interval = interval(Duration::from_secs(13));
            interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

            let sender = Arc::new(sender);

            let f = {
                let sender = sender.clone();

                async move {
                    loop {
                        // TODO: every time a head_block arrives (with a small delay for known slow servers), or on the interval.
                        interval.tick().await;

                        trace!("http interval ready");

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
        let spawn_handles: Vec<_> = server_configs
            .into_iter()
            .map(|(server_name, server_config)| {
                let http_client = http_client.clone();
                let redis_pool = redis_pool.clone();
                let http_interval_sender = http_interval_sender.clone();

                let block_sender = if head_block_sender.is_some() {
                    Some(block_sender.clone())
                } else {
                    None
                };

                let pending_tx_id_sender = Some(pending_tx_id_sender.clone());
                let block_map = block_map.clone();
                let open_request_handle_metrics = open_request_handle_metrics.clone();

                tokio::spawn(async move {
                    server_config
                        .spawn(
                            server_name,
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
                })
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
                    error!(?err, "Unable to create connection");
                }
                Err(err) => {
                    return Err(err.into());
                }
            }
        }

        if connections.len() < min_synced_rpcs {
            return Err(anyhow::anyhow!(
                "Only {}/{} connections!",
                connections.len(),
                min_synced_rpcs
            ));
        }

        // TODO: safety check on sum soft limit

        let synced_connections = SyncedConnections::default();

        // TODO: sizing and expiration on these caches!
        let block_hashes = Cache::builder()
            .time_to_idle(Duration::from_secs(600))
            .max_capacity(10_000)
            .build_with_hasher(ahash::RandomState::new());
        let block_numbers = Cache::builder()
            .time_to_idle(Duration::from_secs(600))
            .max_capacity(10_000)
            .build_with_hasher(ahash::RandomState::new());

        let connections = Arc::new(Self {
            conns: connections,
            synced_connections: ArcSwap::new(Arc::new(synced_connections)),
            pending_transactions,
            block_hashes,
            block_numbers,
            blockchain_graphmap: Default::default(),
            min_sum_soft_limit,
            min_synced_rpcs,
        });

        let handle = {
            let connections = connections.clone();

            tokio::spawn(async move {
                // TODO: try_join_all with the other handles here
                connections
                    .subscribe(
                        pending_tx_id_receiver,
                        block_receiver,
                        head_block_sender,
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
            let handle = task::spawn(async move {
                // TODO: set up this future the same as the block funnel
                while let Ok((pending_tx_id, rpc)) = pending_tx_id_receiver.recv_async().await {
                    let f = clone.clone().process_incoming_tx_id(
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
    #[instrument(skip_all)]
    pub async fn try_send_parallel_requests(
        &self,
        active_request_handles: Vec<OpenRequestHandle>,
        method: &str,
        // TODO: remove this box once i figure out how to do the options
        params: Option<&serde_json::Value>,
    ) -> Result<Box<RawValue>, ProviderError> {
        // TODO: if only 1 active_request_handles, do self.try_send_request?

        let responses = active_request_handles
            .into_iter()
            .map(|active_request_handle| async move {
                let result: Result<Box<RawValue>, _> =
                    active_request_handle.request(method, params, false).await;
                result
            })
            .collect::<FuturesUnordered<_>>()
            .collect::<Vec<Result<Box<RawValue>, ProviderError>>>()
            .await;

        // TODO: Strings are not great keys, but we can't use RawValue or ProviderError as keys because they don't implement Hash or Eq
        let mut count_map: HashMap<String, Result<Box<RawValue>, ProviderError>> = HashMap::new();
        let mut counts: Counter<String> = Counter::new();
        let mut any_ok = false;
        for response in responses {
            // TODO: i think we need to do something smarter with provider error. we at least need to wrap it up as JSON
            // TODO: emit stats errors?
            let s = format!("{:?}", response);

            if count_map.get(&s).is_none() {
                if response.is_ok() {
                    any_ok = true;
                }

                count_map.insert(s.clone(), response);
            }

            counts.update([s].into_iter());
        }

        for (most_common, _) in counts.most_common_ordered() {
            let most_common = count_map.remove(&most_common).unwrap();

            if any_ok && most_common.is_err() {
                //  errors were more common, but we are going to skip them because we got an okay
                continue;
            } else {
                // return the most common
                return most_common;
            }
        }

        // TODO: what should we do if we get here? i don't think we will
        panic!("i don't think this is possible")
    }

    /// get the best available rpc server
    #[instrument(skip_all)]
    pub async fn next_upstream_server(
        &self,
        skip: &[Arc<Web3Connection>],
        min_block_needed: Option<&U64>,
    ) -> anyhow::Result<OpenRequestResult> {
        let mut earliest_retry_at = None;

        // filter the synced rpcs
        // TODO: we are going to be checking "has_block_data" a lot now. i think we pretty much always have min_block_needed now that we override "latest"
        let mut synced_rpcs: Vec<Arc<Web3Connection>> =
            if let Some(min_block_needed) = min_block_needed {
                self.conns
                    .values()
                    .filter(|x| !skip.contains(x))
                    .filter(|x| x.has_block_data(min_block_needed))
                    .cloned()
                    .collect()
            } else {
                self.synced_connections
                    .load()
                    .conns
                    .iter()
                    .filter(|x| !skip.contains(x))
                    .cloned()
                    .collect()
            };

        if synced_rpcs.is_empty() {
            // TODO: what should happen here? automatic retry?
            // TODO: more detailed error
            return Err(anyhow::anyhow!("not synced"));
        }

        // we sort on a bunch of values. cache them here so that we don't do this math multiple times.
        let sort_cache: HashMap<_, _> = synced_rpcs
            .iter()
            .map(|rpc| {
                // TODO: get active requests and the soft limit out of redis?
                let weight = rpc.weight;
                let active_requests = rpc.active_requests();
                let soft_limit = rpc.soft_limit;

                let utilization = active_requests as f32 / soft_limit as f32;

                // TODO: double check this sorts how we want
                (rpc.clone(), (weight, utilization, Reverse(soft_limit)))
            })
            .collect();

        synced_rpcs.sort_unstable_by(|a, b| {
            let a_sorts = sort_cache.get(a).expect("sort_cache should always have a");
            let b_sorts = sort_cache.get(b).expect("sort_cache should always have b");

            // partial_cmp because we are comparing floats
            a_sorts.partial_cmp(b_sorts).unwrap_or(cmp::Ordering::Equal)
        });

        // now that the rpcs are sorted, try to get an active request handle for one of them
        for rpc in synced_rpcs.into_iter() {
            // increment our connection counter
            match rpc.try_request_handle().await {
                Ok(OpenRequestResult::Handle(handle)) => {
                    trace!("next server on {:?}: {:?}", self, rpc);
                    return Ok(OpenRequestResult::Handle(handle));
                }
                Ok(OpenRequestResult::RetryAt(retry_at)) => {
                    earliest_retry_at = earliest_retry_at.min(Some(retry_at));
                }
                Ok(OpenRequestResult::RetryNever) => {
                    // TODO: log a warning?
                }
                Err(err) => {
                    // TODO: log a warning?
                    warn!(?err, "No request handle for {}", rpc)
                }
            }
        }

        warn!("no servers on {:?}! {:?}", self, earliest_retry_at);

        match earliest_retry_at {
            None => todo!(),
            Some(earliest_retry_at) => Ok(OpenRequestResult::RetryAt(earliest_retry_at)),
        }
    }

    /// get all rpc servers that are not rate limited
    /// returns servers even if they aren't in sync. This is useful for broadcasting signed transactions
    // TODO: better type on this that can return an anyhow::Result
    pub async fn upstream_servers(
        &self,
        block_needed: Option<&U64>,
    ) -> Result<Vec<OpenRequestHandle>, Option<Instant>> {
        let mut earliest_retry_at = None;
        // TODO: with capacity?
        let mut selected_rpcs = vec![];

        for connection in self.conns.values() {
            if let Some(block_needed) = block_needed {
                if !connection.has_block_data(block_needed) {
                    continue;
                }
            }

            // check rate limits and increment our connection counter
            match connection.try_request_handle().await {
                Ok(OpenRequestResult::RetryAt(retry_at)) => {
                    // this rpc is not available. skip it
                    earliest_retry_at = earliest_retry_at.min(Some(retry_at));
                }
                Ok(OpenRequestResult::Handle(handle)) => selected_rpcs.push(handle),
                Ok(OpenRequestResult::RetryNever) => {
                    warn!("no request handle for {}", connection)
                }
                Err(err) => {
                    warn!(?err, "error getting request handle for {}", connection)
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
    pub async fn try_send_best_upstream_server(
        &self,
        request: JsonRpcRequest,
        min_block_needed: Option<&U64>,
    ) -> anyhow::Result<JsonRpcForwardedResponse> {
        let mut skip_rpcs = vec![];

        // TODO: maximum retries?
        loop {
            if skip_rpcs.len() == self.conns.len() {
                break;
            }
            match self
                .next_upstream_server(&skip_rpcs, min_block_needed)
                .await?
            {
                OpenRequestResult::Handle(active_request_handle) => {
                    // save the rpc in case we get an error and want to retry on another server
                    skip_rpcs.push(active_request_handle.clone_connection());

                    let response_result = active_request_handle
                        .request(&request.method, &request.params, false)
                        .await;

                    match JsonRpcForwardedResponse::try_from_response_result(
                        response_result,
                        request.id.clone(),
                    ) {
                        Ok(response) => {
                            if let Some(error) = &response.error {
                                trace!(?response, "rpc error");

                                // some errors should be retried on other nodes
                                if error.code == -32000 {
                                    let error_msg = error.message.as_str();

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
                            } else {
                                trace!(?response, "rpc success");
                            }

                            return Ok(response);
                        }
                        Err(e) => {
                            let rpc = skip_rpcs
                                .last()
                                .expect("there must have been a provider if we got an error");

                            warn!(%rpc, ?e, "Backend server error! Retrying on another");

                            // TODO: sleep how long? until synced_connections changes or rate limits are available
                            // sleep(Duration::from_millis(100)).await;

                            continue;
                        }
                    }
                }
                OpenRequestResult::RetryAt(retry_at) => {
                    // TODO: move this to a helper function
                    // sleep (TODO: with a lock?) until our rate limits should be available
                    // TODO: if a server catches up sync while we are waiting, we could stop waiting
                    warn!(?retry_at, "All rate limits exceeded. Sleeping");

                    sleep_until(retry_at).await;

                    continue;
                }
                OpenRequestResult::RetryNever => {
                    warn!(?self, "No server handles!");

                    // TODO: subscribe to something on synced connections. maybe it should just be a watch channel
                    sleep(Duration::from_millis(200)).await;

                    continue;
                }
            }
        }

        Err(anyhow::anyhow!("all {} tries exhausted", skip_rpcs.len()))
    }

    /// be sure there is a timeout on this or it might loop forever
    pub async fn try_send_all_upstream_servers(
        &self,
        request: JsonRpcRequest,
        block_needed: Option<&U64>,
    ) -> anyhow::Result<JsonRpcForwardedResponse> {
        loop {
            match self.upstream_servers(block_needed).await {
                Ok(active_request_handles) => {
                    // TODO: benchmark this compared to waiting on unbounded futures
                    // TODO: do something with this handle?
                    // TODO: this is not working right. simplify
                    let quorum_response = self
                        .try_send_parallel_requests(
                            active_request_handles,
                            request.method.as_ref(),
                            request.params.as_ref(),
                        )
                        .await?;

                    let response = JsonRpcForwardedResponse {
                        jsonrpc: "2.0".to_string(),
                        id: request.id,
                        result: Some(quorum_response),
                        error: None,
                    };

                    return Ok(response);
                }
                Err(None) => {
                    warn!(?self, "No servers in sync! Retrying");

                    // TODO: i don't think this will ever happen
                    // TODO: return a 502? if it does?
                    // return Err(anyhow::anyhow!("no available rpcs!"));
                    // TODO: sleep how long?
                    // TODO: subscribe to something in SyncedConnections instead
                    sleep(Duration::from_millis(200)).await;

                    continue;
                }
                Err(Some(retry_at)) => {
                    // TODO: move this to a helper function
                    // sleep (TODO: with a lock?) until our rate limits should be available
                    // TODO: if a server catches up sync while we are waiting, we could stop waiting
                    warn!("All rate limits exceeded. Sleeping");

                    sleep_until(retry_at).await;

                    continue;
                }
            }
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
        let conns: Vec<&Web3Connection> = self.conns.values().map(|x| x.as_ref()).collect();

        let mut state = serializer.serialize_struct("Web3Connections", 6)?;
        state.serialize_field("conns", &conns)?;
        state.serialize_field("synced_connections", &**self.synced_connections.load())?;
        state.serialize_field("block_hashes_count", &self.block_hashes.entry_count())?;
        state.serialize_field("block_hashes_size", &self.block_hashes.weighted_size())?;
        state.serialize_field("block_numbers_count", &self.block_numbers.entry_count())?;
        state.serialize_field("block_numbers_size", &self.block_numbers.weighted_size())?;
        state.end()
    }
}
