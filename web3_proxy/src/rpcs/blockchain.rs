///! Keep track of the blockchain as seen by a Web3Connections.
use super::connection::Web3Connection;
use super::connections::Web3Connections;
use super::transactions::TxStatus;
use crate::{
    config::BlockAndRpc, jsonrpc::JsonRpcRequest, rpcs::synced_connections::SyncedConnections,
};
use anyhow::Context;
use dashmap::{
    mapref::{entry::Entry, one::Ref},
    DashMap,
};
use derive_more::From;
use ethers::prelude::{Block, TxHash, H256, U64};
use hashbrown::{HashMap, HashSet};
use petgraph::algo::all_simple_paths;
use serde::Serialize;
use serde_json::json;
use std::{cmp::Ordering, fmt::Display, sync::Arc};
use tokio::sync::{broadcast, watch};
use tracing::{debug, info, trace, warn};

pub type ArcBlock = Arc<Block<TxHash>>;

pub type BlockHashesMap = Arc<DashMap<H256, ArcBlock>>;

/// A block's hash and number.
#[derive(Clone, Debug, Default, From, Serialize)]
pub struct BlockId {
    pub(super) hash: H256,
    pub(super) num: U64,
}

impl Display for BlockId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.num, self.hash)
    }
}

impl Web3Connections {
    /// add a block to our map and it's hash to our graphmap of the blockchain
    pub fn save_block(&self, block: &ArcBlock) -> anyhow::Result<()> {
        let block_hash = block.hash.as_ref().context("no block hash")?;
        let block_num = block.number.as_ref().context("no block num")?;
        let _block_td = block
            .total_difficulty
            .as_ref()
            .context("no block total difficulty")?;

        if self.block_hashes.contains_key(block_hash) {
            // this block is already included. no need to continue
            return Ok(());
        }

        let mut blockchain = self.blockchain_graphmap.write();

        if blockchain.contains_node(*block_hash) {
            // this hash is already included. we must have hit that race condition
            // return now since this work was already done.
            return Ok(());
        }

        // TODO: theres a small race between contains_key and insert
        if let Some(_overwritten) = self.block_hashes.insert(*block_hash, block.clone()) {
            // there was a race and another thread wrote this block
            // i don't think this will happen. the blockchain.conains_node above should be enough
            // no need to continue because that other thread would have written (or soon will) write the
            return Ok(());
        }

        match self.block_numbers.entry(*block_num) {
            Entry::Occupied(mut x) => {
                let old = x.insert(*block_hash);

                todo!(
                    "do something with the old hash. we need to update a bunch more block numbers"
                )
            }
            Entry::Vacant(x) => {
                x.insert(*block_hash);
            }
        }

        // TODO: prettier log? or probably move the log somewhere else
        trace!(%block_hash, "new block");

        blockchain.add_node(*block_hash);

        // what should edge weight be? and should the nodes be the blocks instead?
        // TODO: maybe the weight should be the block?
        // we store parent_hash -> hash because the block already stores the parent_hash
        blockchain.add_edge(block.parent_hash, *block_hash, 0);

        // TODO: prune block_numbers and block_map to only keep a configurable (256 on ETH?) number of blocks?

        Ok(())
    }

    /// Get a block from caches with fallback.
    /// Will query a specific node or the best available.
    /// WARNING! This may wait forever. be sure this runs with your own timeout
    pub async fn block(
        &self,
        hash: &H256,
        rpc: Option<&Arc<Web3Connection>>,
    ) -> anyhow::Result<ArcBlock> {
        // first, try to get the hash from our cache
        if let Some(block) = self.block_hashes.get(hash) {
            return Ok(block.clone());
        }

        // block not in cache. we need to ask an rpc for it

        // TODO: helper for method+params => JsonRpcRequest
        // TODO: get block with the transactions?
        // TODO: does this id matter?

        let request_params = (hash, false);

        // TODO: if error, retry?
        let block: Block<TxHash> = match rpc {
            Some(rpc) => {
                rpc.wait_for_request_handle()
                    .await?
                    .request("eth_getBlockByHash", request_params)
                    .await?
            }
            None => {
                let request =
                    json!({ "id": "1", "method": "eth_getBlockByHash", "params": request_params });
                let request: JsonRpcRequest = serde_json::from_value(request)?;

                let response = self.try_send_best_upstream_server(request, None).await?;

                let block = response.result.unwrap();

                serde_json::from_str(block.get())?
            }
        };

        let block = Arc::new(block);

        // the block was fetched using eth_getBlockByHash, so it should have all fields
        self.save_block(&block)?;

        Ok(block)
    }

    /// Convenience method to get the cannonical block at a given block height.
    pub async fn block_hash(&self, num: &U64) -> anyhow::Result<H256> {
        let block = self.cannonical_block(num).await?;

        let hash = block.hash.unwrap();

        Ok(hash)
    }

    /// Get the heaviest chain's block from cache or backend rpc
    pub async fn cannonical_block(&self, num: &U64) -> anyhow::Result<ArcBlock> {
        // we only have blocks by hash now
        // maybe save them during save_block in a blocks_by_number DashMap<U64, Vec<ArcBlock>>
        // if theres multiple, use petgraph to find the one on the main chain (and remove the others if they have enough confirmations)

        // first, try to get the hash from our cache
        if let Some(block_hash) = self.block_numbers.get(num) {
            let block = self
                .block_hashes
                .get(&block_hash)
                .expect("block_numbers gave us this hash");

            return Ok(block.clone());
        }

        // block not in cache. we need to ask an rpc for it
        // but before we do any queries, be sure the requested block num exists
        let head_block_num = self
            .head_block_num()
            .ok_or_else(|| anyhow::anyhow!("no servers in sync"))?;

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
        let request = json!({ "jsonrpc": "2.0", "id": "1", "method": "eth_getBlockByNumber", "params": (num, false) });
        let request: JsonRpcRequest = serde_json::from_value(request)?;

        // TODO: if error, retry?
        let response = self
            .try_send_best_upstream_server(request, Some(num))
            .await?;

        let block = response.result.unwrap();

        let block: Block<TxHash> = serde_json::from_str(block.get())?;

        let block = Arc::new(block);

        // the block was fetched using eth_getBlockByNumber, so it should have all fields
        self.save_block(&block)?;

        Ok(block)
    }

    pub(super) async fn process_incoming_blocks(
        &self,
        block_receiver: flume::Receiver<BlockAndRpc>,
        // TODO: head_block_sender should be a broadcast_sender like pending_tx_sender
        head_block_sender: watch::Sender<ArcBlock>,
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
        rpc_head_block: ArcBlock,
        rpc: Arc<Web3Connection>,
        head_block_sender: &watch::Sender<ArcBlock>,
        pending_tx_sender: &Option<broadcast::Sender<TxStatus>>,
    ) -> anyhow::Result<()> {
        // add the block to connection_heads
        let rpc_block_id = match (rpc_head_block.hash, rpc_head_block.number) {
            (Some(rpc_head_hash), Some(rpc_head_num)) => {
                if rpc_head_num == U64::zero() {
                    debug!(%rpc, "still syncing");

                    connection_heads.remove(&rpc.name);

                    None
                } else {
                    connection_heads.insert(rpc.name.to_owned(), rpc_head_hash);

                    self.save_block(&rpc_head_block)?;

                    Some(BlockId {
                        hash: rpc_head_hash,
                        num: rpc_head_num,
                    })
                }
            }
            _ => {
                warn!(%rpc, ?rpc_head_block, "Block without number or hash!");

                connection_heads.remove(&rpc.name);

                // don't return yet! self.synced_connections likely needs an update
                None
            }
        };

        // iterate the rpc_map to find the highest_work_block
        let mut checked_heads = HashSet::new();
        let mut highest_work_block: Option<Ref<H256, ArcBlock>> = None;

        for rpc_head_hash in connection_heads.values() {
            if checked_heads.contains(rpc_head_hash) {
                continue;
            }

            checked_heads.insert(rpc_head_hash);

            let rpc_head_block = self.block_hashes.get(rpc_head_hash).unwrap();

            match &rpc_head_block.total_difficulty {
                None => {
                    // no total difficulty
                    // TODO: should we fetch the block here? I think this shouldn't happen
                    warn!(?rpc, %rpc_head_hash, "block is missing total difficulty");
                    continue;
                }
                Some(td) => {
                    if highest_work_block.is_none()
                        || td
                            > highest_work_block
                                .as_ref()
                                .expect("there should always be a block here")
                                .total_difficulty
                                .as_ref()
                                .expect("there should always be total difficulty here")
                    {
                        highest_work_block = Some(rpc_head_block);
                    }
                }
            }
        }

        // clone to release the read lock
        let highest_work_block = highest_work_block.map(|x| x.clone());

        let mut highest_work_block = match highest_work_block {
            None => todo!("no servers are in sync"),
            Some(highest_work_block) => highest_work_block,
        };

        // track names so we don't check the same node multiple times
        let mut heavy_names: HashSet<&String> = HashSet::new();
        // track rpcs so we can build a new SyncedConnections
        let mut heavy_rpcs: Vec<&Arc<Web3Connection>> = vec![];
        // a running total of the soft limits covered by the rpcs
        let mut heavy_sum_soft_limit: u32 = 0;

        // check the highest work block and its parents for a set of rpcs that can serve our request load
        // TODO: loop for how many parent blocks? we don't want to serve blocks that are too far behind
        let blockchain_guard = self.blockchain_graphmap.read();
        for _ in 0..3 {
            let highest_work_hash = highest_work_block.hash.as_ref().unwrap();

            for (rpc_name, rpc_head_hash) in connection_heads.iter() {
                if heavy_names.contains(rpc_name) {
                    // this block is already included
                    continue;
                }

                // TODO: does all_simple_paths make this check?
                if rpc_head_hash == highest_work_hash {
                    if let Some(rpc) = self.conns.get(rpc_name) {
                        heavy_names.insert(rpc_name);
                        heavy_rpcs.push(rpc);
                        heavy_sum_soft_limit += rpc.soft_limit;
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
                        heavy_rpcs.push(rpc);
                        heavy_sum_soft_limit += rpc.soft_limit;
                    }
                }
            }

            // TODO: min_sum_soft_limit as a percentage of total_soft_limit?
            // let min_sum_soft_limit = total_soft_limit / self.min_sum_soft_limit;
            if heavy_sum_soft_limit >= self.min_sum_soft_limit {
                // success! this block has enough nodes on it
                break;
            }
            // else, we need to try the parent block

            trace!(%heavy_sum_soft_limit, ?highest_work_hash, "avoiding thundering herd");

            // // TODO: this automatically queries for parents, but need to rearrange lifetimes to make an await work here
            // highest_work_block = self
            //     .block(&highest_work_block.parent_hash, Some(&rpc))
            //     .await?;
            // we give known stale data just because we don't have enough capacity to serve the latest.
            // TODO: maybe we should delay serving requests if this happens.
            // TODO: don't unwrap. break if none?
            match self.block_hashes.get(&highest_work_block.parent_hash) {
                None => {
                    warn!(
                        "ran out of parents to check. soft limit only {}/{}: {}%",
                        heavy_sum_soft_limit,
                        self.min_sum_soft_limit,
                        heavy_sum_soft_limit * 100 / self.min_sum_soft_limit
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

        let soft_limit_met = heavy_sum_soft_limit >= self.min_sum_soft_limit;
        let num_synced_rpcs = heavy_rpcs.len() as u32;

        let new_synced_connections = if soft_limit_met {
            // we have a heavy large enough to serve traffic
            let head_block_hash = highest_work_block.hash.unwrap();
            let head_block_num = highest_work_block.number.unwrap();

            if num_synced_rpcs < self.min_synced_rpcs {
                // TODO: warn is too loud. if we are first starting, this is expected to happen
                warn!(hash=%head_block_hash, num=?head_block_num, "not enough rpcs are synced to advance");

                SyncedConnections::default()
            } else {
                // TODO: wait until at least most of the rpcs have given their initial block?
                // otherwise, if there is a syncing node that is fast, our first head block might not be good
                // TODO: have a configurable "minimum rpcs" number that we can set

                // TODO: sort by weight and soft limit? do we need an IndexSet, or is a Vec fine?
                let conns = heavy_rpcs.into_iter().cloned().collect();

                let head_block_id = BlockId {
                    hash: head_block_hash,
                    num: head_block_num,
                };

                SyncedConnections {
                    head_block_id: Some(head_block_id),
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

        let heavy_block_id = new_synced_connections.head_block_id.clone();
        let new_synced_connections = Arc::new(new_synced_connections);
        let num_connection_heads = connection_heads.len();
        let total_conns = self.conns.len();

        let old_synced_connections = self.synced_connections.swap(new_synced_connections);

        match (&old_synced_connections.head_block_id, &heavy_block_id) {
            (None, None) => warn!("no servers synced"),
            (None, Some(heavy_block_id)) => {
                debug!(block=%heavy_block_id, %rpc, "first consensus head");
            }
            (Some(_), None) => warn!("no longer synced!"),
            (Some(old_block_id), Some(heavy_block_id)) => {
                match heavy_block_id.num.cmp(&old_block_id.num) {
                    Ordering::Equal => {
                        todo!("handle equal")
                    }
                    Ordering::Less => {
                        todo!("handle less")
                    }
                    Ordering::Greater => {
                        todo!("handle greater")
                    }
                }
            }
        }
        /*
        if old_synced_connections.head_block_id.is_none() && rpc_head_block.hash.is_some() {
            // this is fine. we have our first hash
        } else if rpc_head_block.hash.is_some()
            && old_synced_connections.head_block_id.is_some()
            && old_synced_connections
                .head_block_id
                .as_ref()
                .map_ok(|x| x.num)
                != rpc_head_block.hash
        {
            info!(new=%rpc_head_block.hash.unwrap(), new_num=?rpc_head_block.number.unwrap(), heavy=?heavy_block_id, %rpc, "non heavy head");
            // TODO: anything else to do? maybe warn if these blocks are very far apart or forked for an extended period of time
            // TODO: if there is any non-heavy head log how many nodes are on it
        } */

        /*
        if heavy_block_num == U64::zero {
            warn!(?soft_limit_met, %heavy_block_hash, %old_head_hash, %rpc, "NO heavy head  {}/{}/{}", num_synced_rpcs, num_connection_heads, total_rpcs)
        } else if heavy_block_hash == old_head_hash {
            debug!(hash=%heavy_block_hash, num=%heavy_block_num, limit=%heavy_sum_soft_limit, %rpc, "cur heavy head {}/{}/{}", num_synced_rpcs, num_connection_heads, total_rpcs);
        } else if soft_limit_met {
            // TODO: if new's parent is not old, warn?

            debug!(hash=%heavy_block_hash, num=%heavy_block_num, limit=%heavy_sum_soft_limit, %rpc, "NEW heavy head {}/{}/{}", num_synced_rpcs, num_connection_heads, total_rpcs);

            // the head hash changed. forward to any subscribers
            head_block_sender.send(highest_work_block)?;

            // TODO: do something with pending_tx_sender
        } else {
            // TODO: i don't think we can get here
            warn!(?soft_limit_met, %heavy_block_id, %old_head_hash, %rpc, "NO heavy head  {}/{}/{}", num_synced_rpcs, num_connection_heads, total_rpcs)
        }
        */

        // TODO: the head hash changed. forward to any subscribers
        head_block_sender.send(highest_work_block)?;

        Ok(())
    }
}
