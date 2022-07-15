///! Load balanced communication with a group of web3 providers
use anyhow::Context;
use arc_swap::ArcSwap;
use counter::Counter;
use dashmap::DashMap;
use derive_more::From;
use ethers::prelude::{Block, ProviderError, Transaction, TxHash, H256};
use futures::future::try_join_all;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use hashbrown::HashMap;
use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;
use serde_json::value::RawValue;
use std::cmp;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, watch};
use tokio::task;
use tokio::time::{interval, sleep, MissedTickBehavior};
use tracing::{debug, error, info, info_span, instrument, trace, warn};

use crate::app::{flatten_handle, AnyhowJoinHandle, TxState};
use crate::config::Web3ConnectionConfig;
use crate::connection::{ActiveRequestHandle, Web3Connection};
use crate::jsonrpc::{JsonRpcForwardedResponse, JsonRpcRequest};

// Serialize so we can print it on our debug endpoint
#[derive(Clone, Default, Serialize)]
struct SyncedConnections {
    head_block_num: u64,
    head_block_hash: H256,
    // TODO: this should be able to serialize, but it isn't
    #[serde(skip_serializing)]
    inner: BTreeSet<Arc<Web3Connection>>,
    // TODO: use petgraph for keeping track of the chain so we can do better fork handling
}

impl fmt::Debug for SyncedConnections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("SyncedConnections")
            .field("head_block_num", &self.head_block_num)
            .field("head_block_hash", &self.head_block_hash)
            .finish_non_exhaustive()
    }
}

impl SyncedConnections {
    pub fn get_head_block_hash(&self) -> &H256 {
        &self.head_block_hash
    }

    pub fn get_head_block_num(&self) -> u64 {
        self.head_block_num
    }
}

/// A collection of web3 connections. Sends requests either the current best server or all servers.
#[derive(From)]
pub struct Web3Connections {
    inner: Vec<Arc<Web3Connection>>,
    synced_connections: ArcSwap<SyncedConnections>,
    pending_transactions: Arc<DashMap<TxHash, TxState>>,
}

impl Serialize for Web3Connections {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let inner: Vec<&Web3Connection> = self.inner.iter().map(|x| x.as_ref()).collect();

        // 3 is the number of fields in the struct.
        let mut state = serializer.serialize_struct("Web3Connections", 2)?;
        state.serialize_field("rpcs", &inner)?;
        state.serialize_field("synced_connections", &**self.synced_connections.load())?;
        state.end()
    }
}

impl fmt::Debug for Web3Connections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("Web3Connections")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl Web3Connections {
    // #[instrument(name = "spawn_Web3Connections", skip_all)]
    pub async fn spawn(
        chain_id: usize,
        server_configs: Vec<Web3ConnectionConfig>,
        http_client: Option<&reqwest::Client>,
        redis_client_pool: Option<&redis_cell_client::RedisClientPool>,
        head_block_sender: Option<watch::Sender<Block<TxHash>>>,
        pending_tx_sender: Option<broadcast::Sender<TxState>>,
        pending_transactions: Arc<DashMap<TxHash, TxState>>,
    ) -> anyhow::Result<(Arc<Self>, AnyhowJoinHandle<()>)> {
        let num_connections = server_configs.len();

        // TODO: try_join_all
        let mut handles = vec![];

        // TODO: only create these if head_block_sender and pending_tx_sender are set
        let (pending_tx_id_sender, pending_tx_id_receiver) = flume::unbounded();
        let (block_sender, block_receiver) = flume::unbounded();

        let http_interval_sender = if http_client.is_some() {
            let (sender, receiver) = broadcast::channel(1);

            drop(receiver);

            // TODO: what interval?
            let mut interval = interval(Duration::from_secs(13));
            interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

            let sender = Arc::new(sender);

            let f = {
                let sender = sender.clone();

                async move {
                    loop {
                        // TODO: every time a head_block arrives (maybe with a small delay), or on the interval.
                        interval.tick().await;

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

        // turn configs into connections
        let mut connections = Vec::with_capacity(num_connections);
        for server_config in server_configs.into_iter() {
            match server_config
                .spawn(
                    redis_client_pool,
                    chain_id,
                    http_client,
                    http_interval_sender.clone(),
                    Some(block_sender.clone()),
                    Some(pending_tx_id_sender.clone()),
                )
                .await
            {
                Ok((connection, connection_handle)) => {
                    handles.push(flatten_handle(connection_handle));
                    connections.push(connection)
                }
                // TODO: include the server url in this
                Err(e) => warn!("Unable to connect to a server! {:?}", e),
            }
        }

        // TODO: less than 3? what should we do here?
        if connections.len() < 2 {
            warn!("Only {} connection(s)!", connections.len());
        }

        let synced_connections = SyncedConnections::default();

        let connections = Arc::new(Self {
            inner: connections,
            synced_connections: ArcSwap::new(Arc::new(synced_connections)),
            pending_transactions,
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

    async fn _funnel_transaction(
        &self,
        rpc: Arc<Web3Connection>,
        pending_tx_id: TxHash,
    ) -> Result<Option<TxState>, ProviderError> {
        // TODO: yearn devs have had better luck with batching these, but i think that's likely just adding a delay itself
        // TODO: there is a race here on geth. sometimes the rpc isn't yet ready to serve the transaction (even though they told us about it!)
        // TODO: maximum wait time
        let pending_transaction: Transaction = match rpc.try_request_handle().await {
            Ok(request_handle) => {
                request_handle
                    .request("eth_getTransactionByHash", (pending_tx_id,))
                    .await?
            }
            Err(err) => {
                trace!(
                    ?pending_tx_id,
                    ?rpc,
                    ?err,
                    "cancelled funneling transaction"
                );
                return Ok(None);
            }
        };

        trace!(?pending_transaction, "pending");

        match &pending_transaction.block_hash {
            Some(_block_hash) => {
                // the transaction is already confirmed. no need to save in the pending_transactions map
                Ok(Some(TxState::Confirmed(pending_transaction)))
            }
            None => Ok(Some(TxState::Pending(pending_transaction))),
        }
    }

    async fn funnel_transaction(
        self: Arc<Self>,
        rpc: Arc<Web3Connection>,
        pending_tx_id: TxHash,
        pending_tx_sender: broadcast::Sender<TxState>,
    ) -> anyhow::Result<()> {
        // TODO: how many retries? until some timestamp is hit is probably better. maybe just loop and call this with a timeout
        // TODO: after more investigation, i don't think retries will help. i think this is because chains of transactions get dropped from memory
        // TODO: also check the "confirmed transactions" mapping? maybe one shared mapping with TxState in it?
        trace!(?pending_tx_id, "checking pending_transactions on {}", rpc);

        if pending_tx_sender.receiver_count() == 0 {
            // no receivers, so no point in querying to get the full transaction
            return Ok(());
        }

        if self.pending_transactions.contains_key(&pending_tx_id) {
            // this transaction has already been processed
            return Ok(());
        }

        // query the rpc for this transaction
        // it is possible that another rpc is also being queried. thats fine. we want the fastest response
        match self._funnel_transaction(rpc.clone(), pending_tx_id).await {
            Ok(Some(tx_state)) => {
                let _ = pending_tx_sender.send(tx_state);

                trace!(?pending_tx_id, "sent");

                // we sent the transaction. return now. don't break looping because that gives a warning
                return Ok(());
            }
            Ok(None) => {}
            Err(err) => {
                trace!(?err, ?pending_tx_id, "failed fetching transaction");
                // unable to update the entry. sleep and try again soon
                // TODO: retry with exponential backoff with jitter starting from a much smaller time
                // sleep(Duration::from_millis(100)).await;
            }
        }

        // warn is too loud. this is somewhat common
        // "There is a Pending txn with a lower account nonce. This txn can only be executed after confirmation of the earlier Txn Hash#"
        // sometimes it's been pending for many hours
        // sometimes it's maybe something else?
        debug!(?pending_tx_id, "not found on {}", rpc);
        Ok(())
    }

    /// subscribe to all the backend rpcs
    async fn subscribe(
        self: Arc<Self>,
        pending_tx_id_receiver: flume::Receiver<(TxHash, Arc<Web3Connection>)>,
        block_receiver: flume::Receiver<(Block<TxHash>, Arc<Web3Connection>)>,
        head_block_sender: Option<watch::Sender<Block<TxHash>>>,
        pending_tx_sender: Option<broadcast::Sender<TxState>>,
    ) -> anyhow::Result<()> {
        let mut futures = vec![];

        // setup the transaction funnel
        // it skips any duplicates (unless they are being orphaned)
        // fetches new transactions from the notifying rpc
        // forwards new transacitons to pending_tx_receipt_sender
        if let Some(pending_tx_sender) = pending_tx_sender.clone() {
            // TODO: do something with the handle so we can catch any errors
            let clone = self.clone();
            let handle = task::spawn(async move {
                while let Ok((pending_tx_id, rpc)) = pending_tx_id_receiver.recv_async().await {
                    // TODO: spawn this
                    let f = clone.clone().funnel_transaction(
                        rpc,
                        pending_tx_id,
                        pending_tx_sender.clone(),
                    );

                    tokio::spawn(f);
                }

                Ok(())
            });

            futures.push(flatten_handle(handle));
        } else {
            unimplemented!();
        }

        // setup the block funnel
        if let Some(head_block_sender) = head_block_sender {
            let connections = Arc::clone(&self);
            let pending_tx_sender = pending_tx_sender.clone();
            let handle = task::Builder::default()
                .name("update_synced_rpcs")
                .spawn(async move {
                    connections
                        .update_synced_rpcs(block_receiver, head_block_sender, pending_tx_sender)
                        .await
                });

            futures.push(flatten_handle(handle));
        }

        if futures.is_empty() {
            // no transaction or block subscriptions.
            unimplemented!("every second, check that the provider is still connected");
        }

        if let Err(e) = try_join_all(futures).await {
            error!("subscriptions over: {:?}", self);
            return Err(e);
        }

        info!("subscriptions over: {:?}", self);

        Ok(())
    }

    pub fn get_head_block_num(&self) -> u64 {
        self.synced_connections.load().get_head_block_num()
    }

    pub fn get_head_block_hash(&self) -> H256 {
        *self.synced_connections.load().get_head_block_hash()
    }

    pub fn has_synced_rpcs(&self) -> bool {
        !self.synced_connections.load().inner.is_empty()
    }

    pub fn num_synced_rpcs(&self) -> usize {
        self.synced_connections.load().inner.len()
    }

    /// Send the same request to all the handles. Returning the most common success or most common error.
    #[instrument(skip_all)]
    pub async fn try_send_parallel_requests(
        &self,
        active_request_handles: Vec<ActiveRequestHandle>,
        method: &str,
        // TODO: remove this box once i figure out how to do the options
        params: Option<&serde_json::Value>,
    ) -> Result<Box<RawValue>, ProviderError> {
        // TODO: if only 1 active_request_handles, do self.try_send_request

        let responses = active_request_handles
            .into_iter()
            .map(|active_request_handle| async move {
                let result: Result<Box<RawValue>, _> =
                    active_request_handle.request(method, params).await;
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

    /// TODO: possible dead lock here. investigate more. probably refactor
    /// TODO: move parts of this onto SyncedConnections?
    // we don't instrument here because we put a span inside the while loop
    async fn update_synced_rpcs(
        &self,
        block_receiver: flume::Receiver<(Block<TxHash>, Arc<Web3Connection>)>,
        head_block_sender: watch::Sender<Block<TxHash>>,
        // TODO: use pending_tx_sender
        pending_tx_sender: Option<broadcast::Sender<TxState>>,
    ) -> anyhow::Result<()> {
        let total_rpcs = self.inner.len();

        let mut connection_states: HashMap<Arc<Web3Connection>, _> =
            HashMap::with_capacity(total_rpcs);

        let mut pending_synced_connections = SyncedConnections::default();

        while let Ok((new_block, rpc)) = block_receiver.recv_async().await {
            let new_block_num = match new_block.number {
                Some(x) => x.as_u64(),
                None => {
                    // block without a number is expected a node is syncing or
                    if new_block.hash.is_some() {
                        // this seems unlikely, but i'm pretty sure we see it
                        warn!(?new_block, "Block without number!");
                    }
                    continue;
                }
            };
            let new_block_hash = new_block.hash.unwrap();

            // TODO: span with more in it?
            // TODO: make sure i'm doing this span right
            // TODO: show the actual rpc url?
            let span = info_span!("block_receiver", ?rpc, new_block_num);

            // TODO: clippy lint to make sure we don't hold this across an awaited future
            let _enter = span.enter();

            if new_block_num == 0 {
                warn!("still syncing");
            }

            connection_states.insert(rpc.clone(), (new_block_num, new_block_hash));

            // TODO: do something to update the synced blocks
            match new_block_num.cmp(&pending_synced_connections.head_block_num) {
                cmp::Ordering::Greater => {
                    // the rpc's newest block is the new overall best block
                    // TODO: if trace, do the full block hash?
                    // TODO: only accept this block if it is a child of the current head_block
                    info!("new head: {}", new_block_hash);

                    pending_synced_connections.inner.clear();
                    pending_synced_connections.inner.insert(rpc);

                    pending_synced_connections.head_block_num = new_block_num;

                    // TODO: if the parent hash isn't our previous best block, ignore it
                    pending_synced_connections.head_block_hash = new_block_hash;

                    head_block_sender
                        .send(new_block.clone())
                        .context("head_block_sender")?;

                    // TODO: mark all transactions as confirmed
                    // TODO: mark any orphaned transactions as unconfirmed
                }
                cmp::Ordering::Equal => {
                    if new_block_hash == pending_synced_connections.head_block_hash {
                        // this rpc has caught up with the best known head
                        // do not clear synced_connections.
                        // we just want to add this rpc to the end
                        // TODO: HashSet here? i think we get dupes if we don't
                        pending_synced_connections.inner.insert(rpc);
                    } else {
                        // same height, but different chain

                        // check connection_states to see which head block is more popular!
                        let mut rpc_ids_by_block: BTreeMap<H256, Vec<Arc<Web3Connection>>> =
                            BTreeMap::new();

                        let mut counted_rpcs = 0;

                        for (rpc, (block_num, block_hash)) in connection_states.iter() {
                            if *block_num != new_block_num {
                                // this connection isn't synced. we don't care what hash it has
                                continue;
                            }

                            counted_rpcs += 1;

                            let count = rpc_ids_by_block
                                .entry(*block_hash)
                                .or_insert_with(|| Vec::with_capacity(total_rpcs - 1));

                            count.push(rpc.clone());
                        }

                        let most_common_head_hash = *rpc_ids_by_block
                            .iter()
                            .max_by(|a, b| a.1.len().cmp(&b.1.len()))
                            .map(|(k, _v)| k)
                            .unwrap();

                        let synced_rpcs = rpc_ids_by_block.remove(&most_common_head_hash).unwrap();

                        warn!(
                            "chain is forked! {} possible heads. {}/{}/{} rpcs have {}",
                            rpc_ids_by_block.len() + 1,
                            synced_rpcs.len(),
                            counted_rpcs,
                            total_rpcs,
                            most_common_head_hash
                        );

                        // TODO: do this more efficiently?
                        if pending_synced_connections.head_block_hash != most_common_head_hash {
                            head_block_sender
                                .send(new_block.clone())
                                .context("head_block_sender")?;
                            pending_synced_connections.head_block_hash = most_common_head_hash;
                        }

                        pending_synced_connections.inner = synced_rpcs.into_iter().collect();
                    }
                }
                cmp::Ordering::Less => {
                    // this isn't the best block in the tier. don't do anything
                    if !pending_synced_connections.inner.remove(&rpc) {
                        // we didn't remove anything. nothing more to do
                        continue;
                    }
                    // we removed. don't continue so that we update self.synced_connections
                }
            }

            // the synced connections have changed
            let synced_connections = Arc::new(pending_synced_connections.clone());

            if synced_connections.inner.len() == total_rpcs {
                // TODO: more metrics
                trace!("all head: {}", new_block_hash);
            }

            trace!(
                "rpcs at {}: {:?}",
                synced_connections.head_block_hash,
                synced_connections.inner
            );

            // TODO: what if the hashes don't match?
            if pending_synced_connections.head_block_hash == new_block_hash {
                // mark all transactions in the block as confirmed
                if pending_tx_sender.is_some() {
                    for tx_hash in &new_block.transactions {
                        // TODO: should we mark as confirmed via pending_tx_sender?
                        // TODO: possible deadlock here!
                        // trace!("removing {}...", tx_hash);
                        let _ = self.pending_transactions.remove(tx_hash);
                        // trace!("removed {}", tx_hash);
                    }
                };

                // TODO: mark any orphaned transactions as unconfirmed
            }

            // TODO: only publish if there are x (default 2) nodes synced to this block?
            // TODO: do this before or after processing all the transactions in this block?
            self.synced_connections.swap(synced_connections);
        }

        // TODO: if there was an error, we should return it
        warn!("block_receiver exited!");

        Ok(())
    }

    /// get the best available rpc server
    #[instrument(skip_all)]
    pub async fn next_upstream_server(
        &self,
        skip: &[Arc<Web3Connection>],
        archive_needed: bool,
    ) -> Result<ActiveRequestHandle, Option<Duration>> {
        let mut earliest_retry_after = None;

        // filter the synced rpcs
        let mut synced_rpcs: Vec<Arc<Web3Connection>> = if archive_needed {
            // TODO: this includes ALL archive servers. but we only want them if they are on a somewhat recent block
            // TODO: maybe instead of "archive_needed" bool it should be the minimum height. then even delayed servers might be fine. will need to track all heights then
            self.inner
                .iter()
                .filter(|x| x.is_archive())
                .filter(|x| !skip.contains(x))
                .cloned()
                .collect()
        } else {
            self.synced_connections
                .load()
                .inner
                .iter()
                .filter(|x| !skip.contains(x))
                .cloned()
                .collect()
        };

        if synced_rpcs.is_empty() {
            return Err(None);
        }

        let sort_cache: HashMap<Arc<Web3Connection>, (f32, u32)> = synced_rpcs
            .iter()
            .map(|rpc| {
                let active_requests = rpc.active_requests();
                let soft_limit = rpc.soft_limit();

                let utilization = active_requests as f32 / soft_limit as f32;

                (rpc.clone(), (utilization, soft_limit))
            })
            .collect();

        synced_rpcs.sort_unstable_by(|a, b| {
            let (a_utilization, a_soft_limit) = sort_cache.get(a).unwrap();
            let (b_utilization, b_soft_limit) = sort_cache.get(b).unwrap();

            // TODO: i'm comparing floats. crap
            match a_utilization
                .partial_cmp(b_utilization)
                .unwrap_or(cmp::Ordering::Equal)
            {
                cmp::Ordering::Equal => a_soft_limit.cmp(b_soft_limit),
                x => x,
            }
        });

        // now that the rpcs are sorted, try to get an active request handle for one of them
        for rpc in synced_rpcs.into_iter() {
            // increment our connection counter
            match rpc.try_request_handle().await {
                Err(retry_after) => {
                    earliest_retry_after = earliest_retry_after.min(Some(retry_after));
                }
                Ok(handle) => {
                    trace!("next server on {:?}: {:?}", self, rpc);
                    return Ok(handle);
                }
            }
        }

        warn!("no servers on {:?}! {:?}", self, earliest_retry_after);

        // this might be None
        Err(earliest_retry_after)
    }

    /// get all rpc servers that are not rate limited
    /// returns servers even if they aren't in sync. This is useful for broadcasting signed transactions
    pub async fn get_upstream_servers(
        &self,
        archive_needed: bool,
    ) -> Result<Vec<ActiveRequestHandle>, Option<Duration>> {
        let mut earliest_retry_after = None;
        // TODO: with capacity?
        let mut selected_rpcs = vec![];

        for connection in self.inner.iter() {
            if archive_needed && !connection.is_archive() {
                continue;
            }

            // check rate limits and increment our connection counter
            match connection.try_request_handle().await {
                Err(retry_after) => {
                    // this rpc is not available. skip it
                    earliest_retry_after = earliest_retry_after.min(Some(retry_after));
                }
                Ok(handle) => selected_rpcs.push(handle),
            }
        }

        if !selected_rpcs.is_empty() {
            return Ok(selected_rpcs);
        }

        // return the earliest retry_after (if no rpcs are synced, this will be None)
        Err(earliest_retry_after)
    }

    /// be sure there is a timeout on this or it might loop forever
    pub async fn try_send_best_upstream_server(
        &self,
        request: JsonRpcRequest,
        archive_needed: bool,
    ) -> anyhow::Result<JsonRpcForwardedResponse> {
        let mut skip_rpcs = vec![];

        // TODO: maximum retries?
        loop {
            if skip_rpcs.len() == self.inner.len() {
                break;
            }
            match self.next_upstream_server(&skip_rpcs, archive_needed).await {
                Ok(active_request_handle) => {
                    // save the rpc in case we get an error and want to retry on another server
                    skip_rpcs.push(active_request_handle.clone_connection());

                    let response_result = active_request_handle
                        .request(&request.method, &request.params)
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
                            warn!(?self, ?e, "Backend server error!");

                            // TODO: sleep how long? until synced_connections changes or rate limits are available
                            sleep(Duration::from_millis(100)).await;

                            // TODO: when we retry, depending on the error, skip using this same server
                            // for example, if rpc isn't available on this server, retrying is bad
                            // but if an http error, retrying on same is probably fine
                            continue;
                        }
                    }
                }
                Err(None) => {
                    warn!(?self, "No servers in sync!");

                    // TODO: subscribe to something on synced connections. maybe it should just be a watch channel
                    sleep(Duration::from_millis(200)).await;

                    continue;
                    // return Err(anyhow::anyhow!("no servers in sync"));
                }
                Err(Some(retry_after)) => {
                    // TODO: move this to a helper function
                    // sleep (TODO: with a lock?) until our rate limits should be available
                    // TODO: if a server catches up sync while we are waiting, we could stop waiting
                    warn!(?retry_after, "All rate limits exceeded. Sleeping");

                    sleep(retry_after).await;

                    continue;
                }
            }
        }

        Err(anyhow::anyhow!("all retries exhausted"))
    }

    /// be sure there is a timeout on this or it might loop forever
    pub async fn try_send_all_upstream_servers(
        &self,
        request: JsonRpcRequest,
        archive_needed: bool,
    ) -> anyhow::Result<JsonRpcForwardedResponse> {
        loop {
            match self.get_upstream_servers(archive_needed).await {
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
                    warn!(?self, "No servers in sync!");

                    // TODO: i don't think this will ever happen
                    // TODO: return a 502? if it does?
                    // return Err(anyhow::anyhow!("no available rpcs!"));
                    // TODO: sleep how long?
                    sleep(Duration::from_millis(200)).await;

                    continue;
                }
                Err(Some(retry_after)) => {
                    // TODO: move this to a helper function
                    // sleep (TODO: with a lock?) until our rate limits should be available
                    // TODO: if a server catches up sync while we are waiting, we could stop waiting
                    warn!("All rate limits exceeded. Sleeping");

                    sleep(retry_after).await;

                    continue;
                }
            }
        }
    }
}
