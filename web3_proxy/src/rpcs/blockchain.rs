///! Keep track of the blockchain as seen by a Web3Connections.
use super::connection::Web3Connection;
use super::connections::Web3Connections;
use super::transactions::TxStatus;
use crate::{
    config::BlockAndRpc, jsonrpc::JsonRpcRequest, rpcs::synced_connections::SyncedConnections,
};
use anyhow::Context;
use derive_more::From;
use ethers::prelude::{Block, TxHash, H256, U64};
use hashbrown::{HashMap, HashSet};
use moka::future::Cache;
use serde::Serialize;
use serde_json::json;
use std::{cmp::Ordering, fmt::Display, sync::Arc};
use tokio::sync::{broadcast, watch};
use tracing::{debug, trace, warn};

// TODO: type for Hydrated Blocks with their full transactions?
pub type ArcBlock = Arc<Block<TxHash>>;

pub type BlockHashesCache = Cache<H256, ArcBlock, hashbrown::hash_map::DefaultHashBuilder>;

/// A block's hash and number.
#[derive(Clone, Debug, Default, From, Serialize)]
pub struct BlockId {
    pub hash: H256,
    pub num: U64,
}

impl Display for BlockId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.num, self.hash)
    }
}

impl Web3Connections {
    /// add a block to our map and it's hash to our graphmap of the blockchain
    pub async fn save_block(&self, block: &ArcBlock, heaviest_chain: bool) -> anyhow::Result<()> {
        // TODO: i think we can rearrange this function to make it faster on the hot path
        let block_hash = block.hash.as_ref().context("no block hash")?;

        // skip Block::default()
        if block_hash.is_zero() {
            debug!("Skipping block without hash!");
            return Ok(());
        }

        let block_num = block.number.as_ref().context("no block num")?;

        let mut blockchain = self.blockchain_graphmap.write().await;

        // TODO: think more about heaviest_chain
        if heaviest_chain {
            // this is the only place that writes to block_numbers
            // its inside a write lock on blockchain_graphmap, so i think there is no race
            // multiple inserts should be okay though
            self.block_numbers.insert(*block_num, *block_hash).await;
        }

        if blockchain.contains_node(*block_hash) {
            // this hash is already included
            trace!(%block_hash, %block_num, "skipping saving existing block");
            // return now since this work was already done.
            return Ok(());
        }

        // TODO: prettier log? or probably move the log somewhere else
        trace!(%block_hash, %block_num, "saving new block");

        // TODO: theres a small race between contains_key and insert
        self.block_hashes
            .insert(*block_hash, block.to_owned())
            .await;

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
    /// WARNING! If rpc is specified, this may wait forever. be sure this runs with your own timeout
    pub async fn block(
        &self,
        hash: &H256,
        rpc: Option<&Arc<Web3Connection>>,
    ) -> anyhow::Result<ArcBlock> {
        // first, try to get the hash from our cache
        // the cache is set last, so if its here, its everywhere
        if let Some(block) = self.block_hashes.get(hash) {
            return Ok(block);
        }

        // block not in cache. we need to ask an rpc for it
        let get_block_params = (hash, false);
        // TODO: if error, retry?
        let block: Block<TxHash> = match rpc {
            Some(rpc) => {
                rpc.wait_for_request_handle()
                    .await?
                    .request("eth_getBlockByHash", get_block_params, false)
                    .await?
            }
            None => {
                // TODO: helper for method+params => JsonRpcRequest
                // TODO: does this id matter?
                let request = json!({ "id": "1", "method": "eth_getBlockByHash", "params": get_block_params });
                let request: JsonRpcRequest = serde_json::from_value(request)?;

                let response = self.try_send_best_upstream_server(request, None).await?;

                let block = response.result.unwrap();

                serde_json::from_str(block.get())?
            }
        };

        let block = Arc::new(block);

        // the block was fetched using eth_getBlockByHash, so it should have all fields
        // TODO: fill in heaviest_chain! if the block is old enough, is this definitely true?
        self.save_block(&block, false).await?;

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
        // maybe save them during save_block in a blocks_by_number Cache<U64, Vec<ArcBlock>>
        // if theres multiple, use petgraph to find the one on the main chain (and remove the others if they have enough confirmations)

        // be sure the requested block num exists
        let head_block_num = self.head_block_num().context("no servers in sync")?;
        if num > &head_block_num {
            // TODO: i'm seeing this a lot when using ethspam. i dont know why though. i thought we delayed publishing
            // TODO: instead of error, maybe just sleep and try again?
            return Err(anyhow::anyhow!(
                "Head block is #{}, but #{} was requested",
                head_block_num,
                num
            ));
        }

        // try to get the hash from our cache
        // deref to not keep the lock open
        if let Some(block_hash) = self.block_numbers.get(num) {
            // TODO: sometimes this needs to fetch the block. why? i thought block_numbers would only be set if the block hash was set
            return self.block(&block_hash, None).await;
        }

        // block number not in cache. we need to ask an rpc for it
        // TODO: helper for method+params => JsonRpcRequest
        let request = json!({ "jsonrpc": "2.0", "id": "1", "method": "eth_getBlockByNumber", "params": (num, false) });
        let request: JsonRpcRequest = serde_json::from_value(request)?;

        // TODO: if error, retry?
        let response = self
            .try_send_best_upstream_server(request, Some(num))
            .await?;

        let raw_block = response.result.context("no block result")?;

        let block: Block<TxHash> = serde_json::from_str(raw_block.get())?;

        let block = Arc::new(block);

        // the block was fetched using eth_getBlockByNumber, so it should have all fields and be on the heaviest chain
        self.save_block(&block, true).await?;

        Ok(block)
    }

    pub(super) async fn process_incoming_blocks(
        &self,
        block_receiver: flume::Receiver<BlockAndRpc>,
        // TODO: document that this is a watch sender and not a broadcast! if things get busy, blocks might get missed
        // Geth's subscriptions have the same potential for skipping blocks.
        head_block_sender: watch::Sender<ArcBlock>,
        pending_tx_sender: Option<broadcast::Sender<TxStatus>>,
    ) -> anyhow::Result<()> {
        // TODO: indexmap or hashmap? what hasher? with_capacity?
        // TODO: this will grow unbounded. prune old heads on this at the same time we prune the graph?
        let mut connection_heads = HashMap::new();

        while let Ok((new_block, rpc)) = block_receiver.recv_async().await {
            let rpc_name = rpc.name.clone();
            if let Err(err) = self
                .process_block_from_rpc(
                    &mut connection_heads,
                    new_block,
                    rpc,
                    &head_block_sender,
                    &pending_tx_sender,
                )
                .await
            {
                warn!(rpc=%rpc_name, ?err, "unable to process block from rpc");
            }
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
        rpc_head_block: Option<ArcBlock>,
        rpc: Arc<Web3Connection>,
        head_block_sender: &watch::Sender<ArcBlock>,
        pending_tx_sender: &Option<broadcast::Sender<TxStatus>>,
    ) -> anyhow::Result<()> {
        // add the rpc's block to connection_heads, or remove the rpc from connection_heads
        let rpc_head_id = match rpc_head_block {
            Some(rpc_head_block) => {
                let rpc_head_num = rpc_head_block.number.unwrap();
                let rpc_head_hash = rpc_head_block.hash.unwrap();

                if rpc_head_num.is_zero() {
                    // TODO: i don't think we can get to this anymore now that we use Options
                    debug!(%rpc, "still syncing");

                    connection_heads.remove(&rpc.name);

                    None
                } else {
                    // we don't know if its on the heaviest chain yet
                    self.save_block(&rpc_head_block, false).await?;

                    connection_heads.insert(rpc.name.to_owned(), rpc_head_hash);

                    Some(BlockId {
                        hash: rpc_head_hash,
                        num: rpc_head_num,
                    })
                }
            }
            None => {
                // TODO: warn is too verbose. this is expected if a node disconnects and has to reconnect
                trace!(%rpc, "Block without number or hash!");

                connection_heads.remove(&rpc.name);

                None
            }
        };

        // iterate the known heads to find the highest_work_block
        let mut checked_heads = HashSet::new();
        let mut highest_num_block: Option<ArcBlock> = None;
        for (conn_name, connection_head_hash) in connection_heads.iter() {
            if checked_heads.contains(connection_head_hash) {
                // we already checked this head from another rpc
                continue;
            }
            // don't check the same hash multiple times
            checked_heads.insert(connection_head_hash);

            let conn_head_block = if let Some(x) = self.block_hashes.get(connection_head_hash) {
                x
            } else {
                // TODO: why does this happen?!?! seems to only happen with uncled blocks
                // TODO: maybe we should do get_with?
                // TODO: maybe we should just continue. this only seems to happen when an older block is received
                warn!(%connection_head_hash, %conn_name, %rpc, "Missing connection_head_block in block_hashes. Fetching now");

                // this option should always be populated
                let conn_rpc = self.conns.get(conn_name);

                match self.block(connection_head_hash, conn_rpc).await {
                    Ok(block) => block,
                    Err(err) => {
                        warn!(%connection_head_hash, %conn_name, %rpc, ?err, "Failed fetching connection_head_block for block_hashes");
                        continue;
                    }
                }
            };

            match &conn_head_block.number {
                None => {
                    panic!("block is missing number. this is a bug");
                }
                Some(conn_head_num) => {
                    // if this is the first block we've tried
                    // or if this rpc's newest block has a higher number
                    // we used to check total difficulty, but that isn't a thing anymore
                    if highest_num_block.is_none()
                        || conn_head_num
                            > highest_num_block
                                .as_ref()
                                .expect("there should always be a block here")
                                .number
                                .as_ref()
                                .expect("there should always be number here")
                    {
                        highest_num_block = Some(conn_head_block);
                    }
                }
            }
        }

        // clone to release the read lock on self.block_hashes
        if let Some(mut maybe_head_block) = highest_num_block {
            // track rpcs on this heaviest chain so we can build a new SyncedConnections
            let mut highest_rpcs = HashSet::<&String>::new();
            // a running total of the soft limits covered by the rpcs that agree on the head block
            let mut highest_rpcs_sum_soft_limit: u32 = 0;
            // TODO: also track highest_rpcs_sum_hard_limit? llama doesn't need this, so it can wait

            // check the highest work block for a set of rpcs that can serve our request load
            // if it doesn't have enough rpcs for our request load, check the parent block
            // TODO: loop for how many parent blocks? we don't want to serve blocks that are too far behind. probably different per chain
            // TODO: this loop is pretty long. any way to clean up this code?
            for _ in 0..3 {
                let maybe_head_hash = maybe_head_block
                    .hash
                    .as_ref()
                    .expect("blocks here always need hashes");

                // find all rpcs with maybe_head_block as their current head
                for (conn_name, conn_head_hash) in connection_heads.iter() {
                    if conn_head_hash != maybe_head_hash {
                        // connection is not on the desired block
                        continue;
                    }
                    if highest_rpcs.contains(conn_name) {
                        // connection is on a child block
                        continue;
                    }

                    if let Some(rpc) = self.conns.get(conn_name) {
                        highest_rpcs.insert(conn_name);
                        highest_rpcs_sum_soft_limit += rpc.soft_limit;
                    } else {
                        warn!("connection missing")
                    }
                }

                if highest_rpcs_sum_soft_limit < self.min_sum_soft_limit
                    || highest_rpcs.len() < self.min_synced_rpcs
                {
                    // not enough rpcs yet. check the parent
                    if let Some(parent_block) = self.block_hashes.get(&maybe_head_block.parent_hash)
                    {
                        trace!(
                            child=%maybe_head_hash, parent=%parent_block.hash.unwrap(), "avoiding thundering herd",
                        );

                        maybe_head_block = parent_block;
                        continue;
                    } else {
                        warn!(
                            "no parent to check. soft limit only {}/{} from {}/{} rpcs: {}%",
                            highest_rpcs_sum_soft_limit,
                            self.min_sum_soft_limit,
                            highest_rpcs.len(),
                            self.min_synced_rpcs,
                            highest_rpcs_sum_soft_limit * 100 / self.min_sum_soft_limit
                        );
                        break;
                    }
                }
            }

            // TODO: if consensus_head_rpcs.is_empty, try another method of finding the head block

            let num_connection_heads = connection_heads.len();
            let total_conns = self.conns.len();

            // we've done all the searching for the heaviest block that we can
            if highest_rpcs.is_empty() {
                // if we get here, something is wrong. clear synced connections
                let empty_synced_connections = SyncedConnections::default();

                let old_synced_connections = self
                    .synced_connections
                    .swap(Arc::new(empty_synced_connections));

                // TODO: log different things depending on old_synced_connections
                warn!(%rpc, "no consensus head! {}/{}/{}", 0, num_connection_heads, total_conns);
            } else {
                trace!(?highest_rpcs);

                // TODO: if maybe_head_block.time() is old, ignore it

                // success! this block has enough soft limit and nodes on it (or on later blocks)
                let conns: Vec<Arc<Web3Connection>> = highest_rpcs
                    .into_iter()
                    .filter_map(|conn_name| self.conns.get(conn_name).cloned())
                    .collect();

                let consensus_head_block = maybe_head_block;

                let consensus_head_hash = consensus_head_block
                    .hash
                    .expect("head blocks always have hashes");
                let consensus_head_num = consensus_head_block
                    .number
                    .expect("head blocks always have numbers");

                debug_assert_ne!(consensus_head_num, U64::zero());

                let num_consensus_rpcs = conns.len();

                let consensus_head_block_id = BlockId {
                    hash: consensus_head_hash,
                    num: consensus_head_num,
                };

                let new_synced_connections = SyncedConnections {
                    head_block_id: Some(consensus_head_block_id.clone()),
                    conns,
                };

                let old_synced_connections = self
                    .synced_connections
                    .swap(Arc::new(new_synced_connections));

                // TODO: if the rpc_head_block != consensus_head_block, log something?
                match &old_synced_connections.head_block_id {
                    None => {
                        debug!(block=%consensus_head_block_id, %rpc, "first {}/{}/{}", num_consensus_rpcs, num_connection_heads, total_conns);

                        self.save_block(&consensus_head_block, true).await?;

                        head_block_sender.send(consensus_head_block)?;
                    }
                    Some(old_block_id) => {
                        // TODO: do this log item better
                        let rpc_head_str = rpc_head_id
                            .map(|x| x.to_string())
                            .unwrap_or_else(|| "None".to_string());

                        match consensus_head_block_id.num.cmp(&old_block_id.num) {
                            Ordering::Equal => {
                                // TODO: if rpc_block_id != consensus_head_block_id, do a different log?

                                // multiple blocks with the same fork!
                                if consensus_head_block_id.hash == old_block_id.hash {
                                    // no change in hash. no need to use head_block_sender
                                    debug!(con_head=%consensus_head_block_id, rpc_head=%rpc_head_str, %rpc, "con {}/{}/{}", num_consensus_rpcs, num_connection_heads, total_conns)
                                } else {
                                    // hash changed
                                    debug!(con_head=%consensus_head_block_id, old=%old_block_id, rpc_head=%rpc_head_str, %rpc, "unc {}/{}/{}", num_consensus_rpcs, num_connection_heads, total_conns);

                                    // todo!("handle equal by updating the cannonical chain");
                                    self.save_block(&consensus_head_block, true).await?;

                                    head_block_sender.send(consensus_head_block)?;
                                }
                            }
                            Ordering::Less => {
                                // this is unlikely but possible
                                // TODO: better log
                                warn!(con_head=%consensus_head_block_id, old_head=%old_block_id, rpc_head=%rpc_head_str, %rpc, "chain rolled back {}/{}/{}", num_consensus_rpcs, num_connection_heads, total_conns);

                                // TODO: tell save_block to remove any higher block numbers from the cache. not needed because we have other checks on requested blocks being > head, but still seems slike a good idea
                                self.save_block(&consensus_head_block, true).await?;

                                head_block_sender.send(consensus_head_block)?;
                            }
                            Ordering::Greater => {
                                debug!(con_head=%consensus_head_block_id, rpc_head=%rpc_head_str, %rpc, "new {}/{}/{}", num_consensus_rpcs, num_connection_heads, total_conns);

                                self.save_block(&consensus_head_block, true).await?;

                                head_block_sender.send(consensus_head_block)?;
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }
}
