///! Keep track of the blockchain as seen by a Web3Connections.
use super::connection::Web3Connection;
use super::connections::Web3Connections;
use super::transactions::TxStatus;
use crate::{
    config::BlockAndRpc, jsonrpc::JsonRpcRequest, rpcs::synced_connections::SyncedConnections,
};
use dashmap::mapref::one::Ref;
use derive_more::From;
use ethers::prelude::{Block, TxHash, H256, U256, U64};
use hashbrown::{HashMap, HashSet};
use petgraph::algo::all_simple_paths;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::{broadcast, watch};
use tracing::{debug, info, trace, warn};

#[derive(Default, From)]
pub struct BlockId {
    pub(super) hash: H256,
    pub(super) num: U64,
}

/// TODO: do we need this? probably big refactor still to do
pub(super) struct BlockMetadata<'a> {
    pub(super) block: &'a Arc<Block<TxHash>>,
    pub(super) rpc_names: Vec<&'a str>,
    pub(super) sum_soft_limit: u32,
}

impl<'a> BlockMetadata<'a> {
    // TODO: there are sortable traits, but this seems simpler
    /// sort the blocks in descending height
    pub fn sortable_values(&self) -> (&U64, &u32, &U256, &H256) {
        // trace!(?self.block, ?self.conns);

        // first we care about the block number
        let block_num = self.block.number.as_ref().unwrap();

        // if block_num ties, the block with the highest total difficulty *should* be the winner
        // TODO: sometimes i see a block with no total difficulty. websocket subscription doesn't get everything
        // let total_difficulty = self.block.total_difficulty.as_ref().expect("wat");

        // all the nodes should already be doing this fork priority logic themselves
        // so, it should be safe to just look at whatever our node majority thinks and go with that
        let sum_soft_limit = &self.sum_soft_limit;

        let difficulty = &self.block.difficulty;

        // if we are still tied (unlikely). this will definitely break the tie
        // TODO: what does geth do?
        let block_hash = self.block.hash.as_ref().unwrap();

        (block_num, sum_soft_limit, difficulty, block_hash)
    }
}

impl Web3Connections {
    /// adds a block to our map of the blockchain
    pub fn add_block_to_chain(&self, block: &Arc<Block<TxHash>>) -> anyhow::Result<()> {
        let hash = block.hash.ok_or_else(|| anyhow::anyhow!("no block hash"))?;

        if self.block_map.contains_key(&hash) {
            // this block is already included. no need to continue
            return Ok(());
        }

        let mut blockchain = self.blockchain_graphmap.write();

        // TODO: theres a small race between contains_key and insert
        if let Some(overwritten) = self.block_map.insert(hash, block.clone()) {
            // there was a race and another thread wrote this block
            // no need to continue because that other thread would have written (or soon will) write the
            return Ok(());
        }

        if blockchain.contains_node(hash) {
            // this hash is already included. we must have hit another race condition
            // return now since this work was already done.
            return Ok(());
        }

        // TODO: prettier log? or probably move the log somewhere else
        trace!(%hash, "new block");

        // TODO: prune block_map to only keep a configurable (256 on ETH?) number of blocks?

        blockchain.add_node(hash);

        // what should edge weight be? and should the nodes be the blocks instead?
        // TODO: maybe the weight should be the block?
        // we store parent_hash -> hash because the block already stores the parent_hash
        blockchain.add_edge(block.parent_hash, hash, 0);

        Ok(())
    }

    pub async fn block(
        &self,
        hash: &H256,
        rpc: Option<&Web3Connection>,
    ) -> anyhow::Result<Arc<Block<TxHash>>> {
        // first, try to get the hash from our cache
        if let Some(block) = self.block_map.get(hash) {
            return Ok(block.clone());
        }

        // block not in cache. we need to ask an rpc for it

        // TODO: helper for method+params => JsonRpcRequest
        // TODO: get block with the transactions?
        // TODO: does this id matter?
        let request = json!({ "id": "1", "method": "eth_getBlockByHash", "params": (hash, false) });
        let request: JsonRpcRequest = serde_json::from_value(request)?;

        // TODO: if error, retry?
        let response = match rpc {
            Some(rpc) => {
                todo!("send request to this rpc")
            }
            None => self.try_send_best_upstream_server(request, None).await?,
        };

        let block = response.result.unwrap();

        let block: Block<TxHash> = serde_json::from_str(block.get())?;

        let block = Arc::new(block);

        self.add_block_to_chain(&block)?;

        Ok(block)
    }

    /// Convenience method to get the cannonical block at a given block height.
    pub async fn block_hash(&self, num: &U64) -> anyhow::Result<H256> {
        let block = self.cannonical_block(num).await?;

        let hash = block.hash.unwrap();

        Ok(hash)
    }

    /// Get the heaviest chain's block from cache or backend rpc
    pub async fn cannonical_block(&self, num: &U64) -> anyhow::Result<Arc<Block<TxHash>>> {
        todo!();

        /*
        // first, try to get the hash from our cache
        if let Some(block) = self.chain_map.get(num) {
            return Ok(block.clone());
        }

        // block not in cache. we need to ask an rpc for it
        // but before we do any queries, be sure the requested block num exists
        let head_block_num = self.head_block_num();
        if num > &head_block_num {
            // TODO: i'm seeing this a lot when using ethspam. i dont know why though. i thought we delayed publishing
            // TODO: instead of error, maybe just sleep and try again?
            return Err(anyhow::anyhow!(
                "Head block is #{}, but #{} was requested",
                head_block_num,
                num
            ));
        }

        // TODO: helper for method+params => JsonRpcRequest
        // TODO: get block with the transactions?
        let request = json!({ "jsonrpc": "2.0", "id": "1", "method": "eth_getBlockByNumber", "params": (num, false) });
        let request: JsonRpcRequest = serde_json::from_value(request)?;

        // TODO: if error, retry?
        let response = self
            .try_send_best_upstream_server(request, Some(num))
            .await?;

        let block = response.result.unwrap();

        let block: Block<TxHash> = serde_json::from_str(block.get())?;

        let block = Arc::new(block);

        self.add_block(block.clone(), true);

        Ok(block)
        */
    }

    pub(super) async fn process_incoming_blocks(
        &self,
        block_receiver: flume::Receiver<BlockAndRpc>,
        // TODO: head_block_sender should be a broadcast_sender like pending_tx_sender
        head_block_sender: watch::Sender<Arc<Block<TxHash>>>,
        pending_tx_sender: Option<broadcast::Sender<TxStatus>>,
    ) -> anyhow::Result<()> {
        // TODO: indexmap or hashmap? what hasher? with_capacity?
        // TODO: this will grow unbounded. prune old heads on this at the same time we prune the graph?
        let mut connection_heads = HashMap::new();

        while let Ok((new_block, rpc)) = block_receiver.recv_async().await {
            self.process_block_from_rpc(
                &mut connection_heads,
                new_block,
                rpc,
                &head_block_sender,
                &pending_tx_sender,
            )
            .await?;
        }

        // TODO: if there was an error, we should return it
        warn!("block_receiver exited!");

        Ok(())
    }

    /// `connection_heads` is a mapping of rpc_names to head block hashes.
    /// self.blockchain_map is a mapping of hashes to the complete Block<TxHash>.
    /// TODO: return something?
    async fn process_block_from_rpc(
        &self,
        connection_heads: &mut HashMap<String, H256>,
        new_block: Arc<Block<TxHash>>,
        rpc: Arc<Web3Connection>,
        head_block_sender: &watch::Sender<Arc<Block<TxHash>>>,
        pending_tx_sender: &Option<broadcast::Sender<TxStatus>>,
    ) -> anyhow::Result<()> {
        // add the block to connection_heads
        match (new_block.hash, new_block.number) {
            (Some(hash), Some(num)) => {
                if num == U64::zero() {
                    debug!(%rpc, "still syncing");

                    connection_heads.remove(&rpc.name);
                } else {
                    connection_heads.insert(rpc.name.clone(), hash);

                    self.add_block_to_chain(&new_block)?;
                }
            }
            _ => {
                warn!(%rpc, ?new_block, "Block without number or hash!");

                connection_heads.remove(&rpc.name);

                // don't return yet! self.synced_connections likely needs an update
            }
        }

        // iterate the rpc_map to find the highest_work_block
        let mut checked_heads = HashSet::new();
        let mut highest_work_block: Option<Ref<H256, Arc<Block<TxHash>>>> = None;

        for (_rpc_name, rpc_head_hash) in connection_heads.iter() {
            if checked_heads.contains(rpc_head_hash) {
                continue;
            }

            checked_heads.insert(rpc_head_hash);

            let rpc_head_block = self.block_map.get(rpc_head_hash).unwrap();

            if highest_work_block.is_none()
                || rpc_head_block.total_difficulty
                    > highest_work_block.as_ref().unwrap().total_difficulty
            {
                highest_work_block = Some(rpc_head_block);
            }
        }

        // clone to release the read lock
        let highest_work_block = highest_work_block.map(|x| x.clone());

        // TODO: default min_soft_limit? without, we start serving traffic at the start too quickly
        // let min_sum_soft_limit = total_soft_limit / 2;
        // TODO: this should be configurable
        let min_sum_soft_limit = 1;

        let mut highest_work_block = match highest_work_block {
            None => todo!("no servers are in sync"),
            Some(highest_work_block) => highest_work_block,
        };

        // track names so we don't check the same node multiple times
        let mut consensus_names: HashSet<&String> = HashSet::new();
        // track rpcs so we can build a new SyncedConnections
        let mut consensus_rpcs: Vec<&Arc<Web3Connection>> = vec![];
        // a running total of the soft limits covered by the rpcs
        let mut consensus_soft_limit = 0;

        // check the highest work block and its parents for a set of rpcs that can serve our request load
        // TODO: loop for how many parent blocks? we don't want to serve blocks that are too far behind
        let blockchain_guard = self.blockchain_graphmap.read();
        for _ in 0..3 {
            let highest_work_hash = highest_work_block.hash.as_ref().unwrap();

            for (rpc_name, rpc_head_hash) in connection_heads.iter() {
                if consensus_names.contains(rpc_name) {
                    // this block is already included
                    continue;
                }

                // TODO: does all_simple_paths make this check?
                if rpc_head_hash == highest_work_hash {
                    if let Some(rpc) = self.conns.get(rpc_name) {
                        consensus_names.insert(rpc_name);
                        consensus_rpcs.push(rpc);
                        consensus_soft_limit += rpc.soft_limit;
                    }
                    continue;
                }

                // TODO: cache all_simple_paths. there should be a high hit rate
                // TODO: use an algo that saves scratch space?
                // TODO: how slow is this?
                let is_connected = all_simple_paths::<Vec<H256>, _>(
                    &*blockchain_guard,
                    *highest_work_hash,
                    *rpc_head_hash,
                    0,
                    // TODO: what should this max be? probably configurable per chain
                    Some(10),
                )
                .next()
                .is_some();

                if is_connected {
                    if let Some(rpc) = self.conns.get(rpc_name) {
                        consensus_rpcs.push(rpc);
                        consensus_soft_limit += rpc.soft_limit;
                    }
                }
            }

            if consensus_soft_limit >= min_sum_soft_limit {
                // success! this block has enough nodes on it
                break;
            }
            // else, we need to try the parent block

            trace!(%consensus_soft_limit, ?highest_work_hash, "avoiding thundering herd");

            // // TODO: this automatically queries for parents, but need to rearrange lifetimes to make an await work here
            // highest_work_block = self
            //     .block(&highest_work_block.parent_hash, Some(&rpc))
            //     .await?;
            // we give known stale data just because we don't have enough capacity to serve the latest.
            // TODO: maybe we should delay serving requests if this happens.
            // TODO: don't unwrap. break if none?
            match self.block_map.get(&highest_work_block.parent_hash) {
                None => {
                    warn!(
                        "ran out of parents to check. soft limit only {}/{}: {}%",
                        consensus_soft_limit,
                        min_sum_soft_limit,
                        consensus_soft_limit * 100 / min_sum_soft_limit
                    );
                    break;
                }
                Some(parent_block) => {
                    highest_work_block = parent_block.clone();
                }
            }
        }
        // unlock self.blockchain_graphmap
        drop(blockchain_guard);

        let soft_limit_met = consensus_soft_limit >= min_sum_soft_limit;

        let new_synced_connections = if soft_limit_met {
            // we have a consensus large enough to serve traffic
            let head_block_hash = highest_work_block.hash.unwrap();
            let head_block_num = highest_work_block.number.unwrap();
            let num_synced_rpcs = consensus_rpcs.len();

            if num_synced_rpcs < self.min_synced_rpcs {
                trace!(hash=%head_block_hash, num=?head_block_num, "not enough rpcs are synced to advance");

                return Ok(());
            } else {
                // TODO: wait until at least most of the rpcs have given their initial block?
                // otherwise, if there is a syncing node that is fast, our first head block might not be good
                // TODO: have a configurable "minimum rpcs" number that we can set

                // TODO: this logs too much. only log when the hash is first updated?
                debug!(hash=%head_block_hash, num=%head_block_num, rpcs=%num_synced_rpcs, limit=%consensus_soft_limit, "consensus head");

                // TODO: sort by weight and soft limit? do we need an IndexSet, or is a Vec fine?
                let conns = consensus_rpcs.into_iter().cloned().collect();

                SyncedConnections {
                    head_block_num,
                    head_block_hash,
                    conns,
                }
            }
        } else {
            // failure even after checking parent heads!
            // not enough servers are in sync to server traffic
            // TODO: at startup this is fine, but later its a problem
            warn!("empty SyncedConnections");

            SyncedConnections::default()
        };

        let old_synced_connections = Arc::new(new_synced_connections);

        if soft_limit_met && Some(old_synced_connections.head_block_hash) != highest_work_block.hash
        {
            // the head hash changed. forward to any subscribers
            head_block_sender.send(highest_work_block)?;
        }

        Ok(())
    }
}
