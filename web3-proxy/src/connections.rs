///! Load balanced communication with a group of web3 providers
use anyhow::Context;
use arc_swap::ArcSwap;
use counter::Counter;
use dashmap::DashMap;
use derive_more::From;
use ethers::prelude::{Block, ProviderError, Transaction, TxHash, H256, U64};
use futures::future::{join_all, try_join_all};
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use hashbrown::HashMap;
use indexmap::{IndexMap, IndexSet};
// use parking_lot::RwLock;
// use petgraph::graphmap::DiGraphMap;
use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;
use serde_json::json;
use serde_json::value::RawValue;
use std::cmp;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, watch};
use tokio::task;
use tokio::time::{interval, sleep, MissedTickBehavior};
use tracing::{debug, error, info, instrument, trace, warn};

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
    // TODO: use linkedhashmap?
    #[serde(skip_serializing)]
    conns: IndexSet<Arc<Web3Connection>>,
}

impl fmt::Debug for SyncedConnections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("SyncedConnections")
            .field("head_num", &self.head_block_num)
            .field("head_hash", &self.head_block_hash)
            .finish_non_exhaustive()
    }
}

impl SyncedConnections {
    pub fn get_head_block_hash(&self) -> &H256 {
        &self.head_block_hash
    }

    pub fn get_head_block_num(&self) -> U64 {
        self.head_block_num.into()
    }
}

#[derive(Default)]
pub struct BlockChain {
    /// only includes blocks on the main chain.
    chain_map: DashMap<U64, Arc<Block<TxHash>>>,
    /// all blocks, including orphans
    block_map: DashMap<H256, Arc<Block<TxHash>>>,
    // TODO: petgraph?
}

impl BlockChain {
    pub fn add_block(&self, block: Arc<Block<TxHash>>, cannonical: bool) {
        let hash = block.hash.unwrap();

        if cannonical {
            let num = block.number.unwrap();

            let entry = self.chain_map.entry(num);

            let mut is_new = false;

            entry.or_insert_with(|| {
                is_new = true;
                block.clone()
            });

            if !is_new {
                return;
            }
        }

        self.block_map.entry(hash).or_insert(block);
    }

    pub fn get_cannonical_block(&self, num: &U64) -> Option<Arc<Block<TxHash>>> {
        self.chain_map.get(num).map(|x| x.clone())
    }

    pub fn get_block(&self, hash: &H256) -> Option<Arc<Block<TxHash>>> {
        self.block_map.get(hash).map(|x| x.clone())
    }
}

/// A collection of web3 connections. Sends requests either the current best server or all servers.
#[derive(From)]
pub struct Web3Connections {
    conns: IndexMap<String, Arc<Web3Connection>>,
    synced_connections: ArcSwap<SyncedConnections>,
    pending_transactions: Arc<DashMap<TxHash, TxState>>,
    // TODO: i think chain is what we want, but i'm not sure how we'll use it yet
    // TODO: this graph is going to grow forever unless we do some sort of pruning. maybe store pruned in redis?
    // chain: Arc<RwLock<DiGraphMap<H256, Block<TxHash>>>>,
    chain: BlockChain,
}

impl Serialize for Web3Connections {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let conns: Vec<&Web3Connection> = self.conns.iter().map(|x| x.1.as_ref()).collect();

        let mut state = serializer.serialize_struct("Web3Connections", 2)?;
        state.serialize_field("conns", &conns)?;
        state.serialize_field("synced_connections", &**self.synced_connections.load())?;
        state.end()
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

impl Web3Connections {
    // #[instrument(name = "spawn_Web3Connections", skip_all)]
    pub async fn spawn(
        chain_id: u64,
        server_configs: Vec<Web3ConnectionConfig>,
        http_client: Option<reqwest::Client>,
        redis_client_pool: Option<redis_cell_client::RedisClientPool>,
        head_block_sender: Option<watch::Sender<Arc<Block<TxHash>>>>,
        pending_tx_sender: Option<broadcast::Sender<TxState>>,
        pending_transactions: Arc<DashMap<TxHash, TxState>>,
    ) -> anyhow::Result<(Arc<Self>, AnyhowJoinHandle<()>)> {
        let (pending_tx_id_sender, pending_tx_id_receiver) = flume::unbounded();
        let (block_sender, block_receiver) =
            flume::unbounded::<(Arc<Block<H256>>, Arc<Web3Connection>)>();

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
                        // TODO: every time a head_block arrives (maybe with a small delay), or on the interval.
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
        let spawn_handles: Vec<_> = server_configs
            .into_iter()
            .map(|server_config| {
                let http_client = http_client.clone();
                let redis_client_pool = redis_client_pool.clone();
                let http_interval_sender = http_interval_sender.clone();
                let block_sender = Some(block_sender.clone());
                let pending_tx_id_sender = Some(pending_tx_id_sender.clone());

                tokio::spawn(async move {
                    server_config
                        .spawn(
                            redis_client_pool,
                            chain_id,
                            http_client,
                            http_interval_sender,
                            block_sender,
                            pending_tx_id_sender,
                        )
                        .await
                })
            })
            .collect();

        let mut connections = IndexMap::new();
        let mut handles = vec![];

        // TODO: futures unordered?
        for x in join_all(spawn_handles).await {
            // TODO: how should we handle errors here? one rpc being down shouldn't cause the program to exit
            match x {
                Ok(Ok((connection, handle))) => {
                    connections.insert(connection.url().to_string(), connection);
                    handles.push(handle);
                }
                Ok(Err(err)) => {
                    // TODO: some of these are probably retry-able
                    error!(?err);
                }
                Err(err) => {
                    return Err(err.into());
                }
            }
        }

        // TODO: less than 3? what should we do here?
        if connections.len() < 2 {
            warn!("Only {} connection(s)!", connections.len());
        }

        let synced_connections = SyncedConnections::default();

        let connections = Arc::new(Self {
            conns: connections,
            synced_connections: ArcSwap::new(Arc::new(synced_connections)),
            pending_transactions,
            chain: Default::default(),
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

    /// dedupe transaction and send them to any listening clients
    async fn funnel_transaction(
        self: Arc<Self>,
        rpc: Arc<Web3Connection>,
        pending_tx_id: TxHash,
        pending_tx_sender: broadcast::Sender<TxState>,
    ) -> anyhow::Result<()> {
        // TODO: how many retries? until some timestamp is hit is probably better. maybe just loop and call this with a timeout
        // TODO: after more investigation, i don't think retries will help. i think this is because chains of transactions get dropped from memory
        // TODO: also check the "confirmed transactions" mapping? maybe one shared mapping with TxState in it?

        if pending_tx_sender.receiver_count() == 0 {
            // no receivers, so no point in querying to get the full transaction
            return Ok(());
        }

        trace!(?pending_tx_id, "checking pending_transactions on {}", rpc);

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

    /// subscribe to blocks and transactions from all the backend rpcs.
    /// blocks are processed by all the `Web3Connection`s and then sent to the `block_receiver`
    /// transaction ids from all the `Web3Connection`s are deduplicated and forwarded to `pending_tx_sender`
    async fn subscribe(
        self: Arc<Self>,
        pending_tx_id_receiver: flume::Receiver<(TxHash, Arc<Web3Connection>)>,
        block_receiver: flume::Receiver<(Arc<Block<TxHash>>, Arc<Web3Connection>)>,
        head_block_sender: Option<watch::Sender<Arc<Block<TxHash>>>>,
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
            todo!("every second, check that the provider is still connected");
        }

        if let Err(e) = try_join_all(futures).await {
            error!("subscriptions over: {:?}", self);
            return Err(e);
        }

        info!("subscriptions over: {:?}", self);

        Ok(())
    }

    pub async fn get_block(&self, hash: &H256) -> anyhow::Result<Arc<Block<TxHash>>> {
        // first, try to get the hash from our cache
        if let Some(block) = self.chain.get_block(hash) {
            return Ok(block);
        }

        // block not in cache. we need to ask an rpc for it

        // TODO: helper for method+params => JsonRpcRequest
        // TODO: get block with the transactions?
        let request = json!({ "id": "1", "method": "eth_getBlockByHash", "params": (hash, false) });
        let request: JsonRpcRequest = serde_json::from_value(request)?;

        // TODO: if error, retry?
        let response = self.try_send_best_upstream_server(request, None).await?;

        let block = response.result.unwrap();

        let block: Block<TxHash> = serde_json::from_str(block.get())?;

        let block = Arc::new(block);

        self.chain.add_block(block.clone(), false);

        Ok(block)
    }

    /// Get the heaviest chain's block from cache or backend rpc
    pub async fn get_cannonical_block(&self, num: &U64) -> anyhow::Result<Arc<Block<TxHash>>> {
        // first, try to get the hash from our cache
        if let Some(block) = self.chain.get_cannonical_block(num) {
            return Ok(block);
        }

        // block not in cache. we need to ask an rpc for it
        // but before we do any queries, be sure the requested block num exists
        let head_block_num = self.get_head_block_num();
        if num > &head_block_num {
            return Err(anyhow::anyhow!(
                "Head block is #{}, but #{} was requested",
                head_block_num,
                num
            ));
        }

        // TODO: helper for method+params => JsonRpcRequest
        // TODO: get block with the transactions?
        let request =
            json!({ "id": "1", "method": "eth_getBlockByNumber", "params": (num, false) });
        let request: JsonRpcRequest = serde_json::from_value(request)?;

        // TODO: if error, retry?
        let response = self
            .try_send_best_upstream_server(request, Some(num))
            .await?;

        let block = response.result.unwrap();

        let block: Block<TxHash> = serde_json::from_str(block.get())?;

        let block = Arc::new(block);

        self.chain.add_block(block.clone(), true);

        Ok(block)
    }

    /// Convenience method to get the cannonical block at a given block height.
    pub async fn get_block_hash(&self, num: &U64) -> anyhow::Result<H256> {
        let block = self.get_cannonical_block(num).await?;

        let hash = block.hash.unwrap();

        Ok(hash)
    }

    pub fn get_head_block(&self) -> (U64, H256) {
        let synced_connections = self.synced_connections.load();

        let num = synced_connections.get_head_block_num();
        let hash = *synced_connections.get_head_block_hash();

        (num, hash)
    }

    pub fn get_head_block_hash(&self) -> H256 {
        *self.synced_connections.load().get_head_block_hash()
    }

    pub fn get_head_block_num(&self) -> U64 {
        self.synced_connections.load().get_head_block_num()
    }

    pub fn has_synced_rpcs(&self) -> bool {
        if self.synced_connections.load().conns.is_empty() {
            return false;
        }
        self.get_head_block_num() > U64::zero()
    }

    pub fn num_synced_rpcs(&self) -> usize {
        self.synced_connections.load().conns.len()
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
        block_receiver: flume::Receiver<(Arc<Block<TxHash>>, Arc<Web3Connection>)>,
        head_block_sender: watch::Sender<Arc<Block<TxHash>>>,
        // TODO: use pending_tx_sender
        pending_tx_sender: Option<broadcast::Sender<TxState>>,
    ) -> anyhow::Result<()> {
        let total_rpcs = self.conns.len();

        // TODO: rpc name instead of url (will do this with config reload revamp)
        // TODO: indexmap or hashmap? what hasher? with_capacity?
        let mut connection_heads = IndexMap::<String, Arc<Block<TxHash>>>::new();

        while let Ok((new_block, rpc)) = block_receiver.recv_async().await {
            let new_block_hash = if let Some(hash) = new_block.hash {
                hash
            } else {
                warn!(%rpc, ?new_block, "Block without hash!");

                connection_heads.remove(rpc.url());

                continue;
            };

            // TODO: dry this with the code above
            let new_block_num = if let Some(num) = new_block.number {
                num
            } else {
                // this seems unlikely, but i'm pretty sure we have seen it
                // maybe when a node is syncing or reconnecting?
                warn!(%rpc, ?new_block, "Block without number!");

                connection_heads.remove(rpc.url());

                continue;
            };

            // TODO: span with more in it?
            // TODO: make sure i'm doing this span right
            // TODO: show the actual rpc url?
            // TODO: clippy lint to make sure we don't hold this across an awaited future
            // TODO: what level?
            // let _span = info_span!("block_receiver", %rpc, %new_block_num).entered();

            if new_block_num == U64::zero() {
                warn!(%rpc, %new_block_num, "still syncing");

                connection_heads.remove(rpc.url());
            } else {
                // TODO: no clone? we end up with different blocks for every rpc
                connection_heads.insert(rpc.url().to_string(), new_block.clone());

                self.chain.add_block(new_block.clone(), false);
            }

            // iterate connection_heads to find the oldest block
            let lowest_block_num = if let Some(lowest_block) = connection_heads
                .values()
                .min_by(|a, b| a.number.cmp(&b.number))
            {
                lowest_block.number.unwrap()
            } else {
                continue;
            };

            // iterate connection_heads to find the consensus block
            let mut rpcs_by_num = IndexMap::<U64, Vec<&str>>::new();
            let mut blocks_by_hash = IndexMap::<H256, Arc<Block<TxHash>>>::new();
            // block_hash => soft_limit, rpcs
            // TODO: proper type for this?
            let mut rpcs_by_hash = IndexMap::<H256, Vec<&str>>::new();
            let mut total_soft_limit = 0;

            for (rpc_url, head_block) in connection_heads.iter() {
                if let Some(rpc) = self.conns.get(rpc_url) {
                    // we need the total soft limit in order to know when its safe to update the backends
                    total_soft_limit += rpc.soft_limit();

                    let head_hash = head_block.hash.unwrap();

                    // save the block
                    blocks_by_hash
                        .entry(head_hash)
                        .or_insert_with(|| head_block.clone());

                    // add the rpc to all relevant block heights
                    let mut block = head_block.clone();
                    while block.number.unwrap() >= lowest_block_num {
                        let block_hash = block.hash.unwrap();
                        let block_num = block.number.unwrap();

                        // save the rpcs and the sum of their soft limit by their head hash
                        let rpc_urls_by_hash =
                            rpcs_by_hash.entry(block_hash).or_insert_with(Vec::new);

                        rpc_urls_by_hash.push(rpc_url);

                        // save the rpcs by their number
                        let rpc_urls_by_num = rpcs_by_num.entry(block_num).or_insert_with(Vec::new);

                        rpc_urls_by_num.push(rpc_url);

                        if let Some(parent) = self.chain.get_block(&block.parent_hash) {
                            // save the parent block
                            blocks_by_hash.insert(block.parent_hash, parent.clone());

                            block = parent
                        } else {
                            // log this? eventually we will hit a block we don't have, so it's not an error
                            break;
                        }
                    }
                }
            }

            // TODO: configurable threshold. in test we have 4 servers so
            // TODO:
            // TODO: minimum total_soft_limit? without, when one server is in the loop
            let min_soft_limit = total_soft_limit / 2;

            struct State<'a> {
                block: &'a Arc<Block<TxHash>>,
                sum_soft_limit: u32,
                conns: Vec<&'a str>,
            }

            impl<'a> State<'a> {
                // TODO: there are sortable traits, but this seems simpler
                fn sortable_values(&self) -> (&U64, &u32, usize, &H256) {
                    let block_num = self.block.number.as_ref().unwrap();

                    let sum_soft_limit = &self.sum_soft_limit;

                    let conns = self.conns.len();

                    let block_hash = self.block.hash.as_ref().unwrap();

                    (block_num, sum_soft_limit, conns, block_hash)
                }
            }

            trace!(?rpcs_by_hash);

            // TODO: i'm always getting None
            if let Some(x) = rpcs_by_hash
                .into_iter()
                .filter_map(|(hash, conns)| {
                    // TODO: move this to `State::new` function on
                    let sum_soft_limit = conns
                        .iter()
                        .map(|rpc_url| {
                            if let Some(rpc) = self.conns.get(*rpc_url) {
                                rpc.soft_limit()
                            } else {
                                0
                            }
                        })
                        .sum();

                    if sum_soft_limit < min_soft_limit {
                        trace!(?sum_soft_limit, ?min_soft_limit, "sum_soft_limit too low");
                        None
                    } else {
                        let block = blocks_by_hash.get(&hash).unwrap();

                        Some(State {
                            block,
                            sum_soft_limit,
                            conns,
                        })
                    }
                })
                .max_by(|a, b| a.sortable_values().cmp(&b.sortable_values()))
            {
                let best_head_num = x.block.number.unwrap();
                let best_head_hash = x.block.hash.unwrap();
                let best_rpcs = x.conns;

                let synced_rpcs = rpcs_by_num.remove(&best_head_num).unwrap();

                if best_rpcs.len() == synced_rpcs.len() {
                    trace!(
                        "{}/{}/{}/{} rpcs have {}",
                        best_rpcs.len(),
                        synced_rpcs.len(),
                        connection_heads.len(),
                        total_rpcs,
                        best_head_hash
                    );
                } else {
                    warn!(
                        "chain is forked! {} possible heads. {}/{}/{}/{} rpcs have {}",
                        "?", // TODO: how should we get this?
                        best_rpcs.len(),
                        synced_rpcs.len(),
                        connection_heads.len(),
                        total_rpcs,
                        best_head_hash
                    );
                }

                let num_best_rpcs = best_rpcs.len();

                // TODOL: do this without clone?
                let conns = best_rpcs
                    .into_iter()
                    .map(|x| self.conns.get(x).unwrap().clone())
                    .collect();

                let pending_synced_connections = SyncedConnections {
                    head_block_num: best_head_num.as_u64(),
                    head_block_hash: best_head_hash,
                    conns,
                };

                let current_head_block = self.get_head_block_hash();
                let new_head_block =
                    pending_synced_connections.head_block_hash != current_head_block;

                if new_head_block {
                    self.chain.add_block(new_block.clone(), true);

                    info!(
                        "{}/{} rpcs at {} ({}). publishing new head!",
                        pending_synced_connections.conns.len(),
                        self.conns.len(),
                        pending_synced_connections.head_block_hash,
                        pending_synced_connections.head_block_num,
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
                } else {
                    // TODO: i'm seeing 4/4 print twice. maybe because of http providers?
                    // TODO: only do this log if there was a change
                    trace!(
                        "{}/{} rpcs at {} ({})",
                        pending_synced_connections.conns.len(),
                        self.conns.len(),
                        pending_synced_connections.head_block_hash,
                        pending_synced_connections.head_block_num,
                    );
                }

                // TODO: do this before or after processing all the transactions in this block?
                // TODO: only swap if there is a change
                trace!(?pending_synced_connections, "swapping");
                self.synced_connections
                    .swap(Arc::new(pending_synced_connections));

                if new_head_block {
                    // TODO: this will need a refactor to only send once a minmum threshold has this block
                    // TODO: move this onto self.chain
                    // TODO: pending_synced_connections isn't published yet. which means fast queries using this block will fail
                    head_block_sender
                        .send(new_block.clone())
                        .context("head_block_sender")?;
                }
            } else {
                // TODO: this expected when we first start
                // TODO: make sure self.synced_connections is empty
                warn!("not enough rpcs in sync");
            }
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
        min_block_needed: Option<&U64>,
    ) -> Result<ActiveRequestHandle, Option<Duration>> {
        let mut earliest_retry_after = None;

        // filter the synced rpcs
        // TODO: we are going to be checking "has_block_data" a lot now. i think we pretty much always have min_block_needed now that we override "latest"
        let mut synced_rpcs: Vec<Arc<Web3Connection>> =
            if let Some(min_block_needed) = min_block_needed {
                // TODO: this includes ALL archive servers. but we only want them if they are on a somewhat recent block
                // TODO: maybe instead of "archive_needed" bool it should be the minimum height. then even delayed servers might be fine. will need to track all heights then
                self.conns
                    .values()
                    .filter(|x| x.has_block_data(min_block_needed))
                    .filter(|x| !skip.contains(x))
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
            return Err(None);
        }

        let sort_cache: HashMap<_, _> = synced_rpcs
            .iter()
            .map(|rpc| {
                // TODO: get active requests and the soft limit out of redis?
                let active_requests = rpc.active_requests();
                let soft_limit = rpc.soft_limit();
                let block_data_limit = rpc.block_data_limit();

                let utilization = active_requests as f32 / soft_limit as f32;

                // TODO: double check this sorts how we want
                (rpc.clone(), (block_data_limit, utilization, soft_limit))
            })
            .collect();

        synced_rpcs.sort_unstable_by(|a, b| {
            let a_sorts = sort_cache.get(a).unwrap();
            let b_sorts = sort_cache.get(b).unwrap();

            // TODO: i'm comparing floats. crap
            a_sorts.partial_cmp(b_sorts).unwrap_or(cmp::Ordering::Equal)
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
        min_block_needed: Option<&U64>,
    ) -> Result<Vec<ActiveRequestHandle>, Option<Duration>> {
        let mut earliest_retry_after = None;
        // TODO: with capacity?
        let mut selected_rpcs = vec![];

        for connection in self.conns.values() {
            if let Some(min_block_needed) = min_block_needed {
                if !connection.has_block_data(min_block_needed) {
                    continue;
                }
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
                .await
            {
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
                    // TODO: is there some way to check if no servers will ever be in sync?
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
        min_block_needed: Option<&U64>,
    ) -> anyhow::Result<JsonRpcForwardedResponse> {
        loop {
            match self.get_upstream_servers(min_block_needed).await {
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
                    // TODO: subscribe to something in SyncedConnections instead
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

mod tests {
    #[test]
    fn test_false_before_true() {
        let mut x = [true, false, true];

        x.sort_unstable();

        assert_eq!(x, [false, true, true])
    }
}
