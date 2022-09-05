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
    pub fn save_block(&self, block: &ArcBlock, heaviest_chain: Option<bool>) -> anyhow::Result<()> {
        // TODO: i think we can rearrange this function to make it faster on the hot path
        let block_hash = block.hash.as_ref().context("no block hash")?;
        let block_num = block.number.as_ref().context("no block num")?;
        let _block_td = block
            .total_difficulty
            .as_ref()
            .context("no block total difficulty")?;

        // if self.block_hashes.contains_key(block_hash) {
        //     // this block is already included. no need to continue
        //     return Ok(());
        // }

        let mut blockchain = self.blockchain_graphmap.write();

        // think more about heaviest_chain
        if heaviest_chain.unwrap_or(true) {
            match self.block_numbers.entry(*block_num) {
                Entry::Occupied(mut x) => {
                    let old_hash = x.insert(*block_hash);

                    if block_hash == &old_hash {
                        // this block has already been saved
                        return Ok(());
                    }

                    // TODO: what should we do?
                    // TODO: if old_hash's number is > block_num, we need to remove more entries
                    warn!(
                        "do something with the old hash ({}) for {}? we may need to update a bunch more block numbers", old_hash, block_num
                    )
                }
                Entry::Vacant(x) => {
                    x.insert(*block_hash);
                }
            }
        }

        // if blockchain.contains_node(*block_hash) {
        //     // this hash is already included. we must have hit that race condition
        //     // return now since this work was already done.
        //     return Ok(());
        // }

        // TODO: theres a small race between contains_key and insert
        if let Some(_overwritten) = self.block_hashes.insert(*block_hash, block.clone()) {
            // there was a race and another thread wrote this block
            // i don't think this will happen. the blockchain.conains_node above should be enough
            // no need to continue because that other thread would have written (or soon will) write the
            return Ok(());
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
    /// WARNING! If rpc is specified, this may wait forever. be sure this runs with your own timeout
    pub async fn block(
        &self,
        hash: &H256,
        rpc: Option<&Arc<Web3Connection>>,
    ) -> anyhow::Result<ArcBlock> {
        // first, try to get the hash from our cache
        // the cache is set last, so if its here, its everywhere
        if let Some(block) = self.block_hashes.get(hash) {
            return Ok(block.clone());
        }

        // block not in cache. we need to ask an rpc for it
        let get_block_params = (hash, false);
        // TODO: if error, retry?
        let block: Block<TxHash> = match rpc {
            Some(rpc) => {
                rpc.wait_for_request_handle()
                    .await?
                    .request("eth_getBlockByHash", get_block_params)
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
        self.save_block(&block, None)?;

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

        // be sure the requested block num exists
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

        // try to get the hash from our cache
        // deref to not keep the lock open
        if let Some(block_hash) = self.block_numbers.get(num).map(|x| *x) {
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
        self.save_block(&block, Some(true))?;

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

                    // we don't know if its on the heaviest chain yet
                    self.save_block(&rpc_head_block, Some(false))?;

                    Some(BlockId {
                        hash: rpc_head_hash,
                        num: rpc_head_num,
                    })
                }
            }
            _ => {
                // TODO: warn is too verbose. this is expected if a node disconnects and has to reconnect
                trace!(%rpc, "Block without number or hash!");

                connection_heads.remove(&rpc.name);

                // don't return yet! self.synced_connections likely needs an update
                None
            }
        };

        // iterate the known heads to find the highest_work_block
        let mut checked_heads = HashSet::new();
        let mut highest_work_block: Option<Ref<H256, ArcBlock>> = None;
        for conn_head_hash in connection_heads.values() {
            if checked_heads.contains(conn_head_hash) {
                // we already checked this head from another rpc
                continue;
            }
            // don't check the same hash multiple times
            checked_heads.insert(conn_head_hash);

            let rpc_head_block = if let Some(x) = self.block_hashes.get(conn_head_hash) {
                x
            } else {
                // TODO: why does this happen?
                warn!(%conn_head_hash, %rpc, "No block found");
                continue;
            };

            match &rpc_head_block.total_difficulty {
                None => {
                    // no total difficulty. this is a bug
                    unimplemented!("block is missing total difficulty");
                }
                Some(td) => {
                    // if this is the first block we've tried
                    // or if this rpc's newest block has a higher total difficulty
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

        // clone to release the read lock on self.block_hashes
        if let Some(mut maybe_head_block) = highest_work_block.map(|x| x.clone()) {
            // track rpcs on this heaviest chain so we can build a new SyncedConnections
            let mut heavy_rpcs: Vec<&Arc<Web3Connection>> = vec![];
            // a running total of the soft limits covered by the heavy rpcs
            let mut heavy_sum_soft_limit: u32 = 0;
            // TODO: also track heavy_sum_hard_limit?

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
                        continue;
                    }

                    if let Some(rpc) = self.conns.get(conn_name) {
                        heavy_rpcs.push(rpc);
                        heavy_sum_soft_limit += rpc.soft_limit;
                    } else {
                        warn!("connection missing")
                    }
                }

                if heavy_sum_soft_limit < self.min_sum_soft_limit
                    || heavy_rpcs.len() < self.min_synced_rpcs
                {
                    // not enough rpcs yet. check the parent
                    if let Some(parent_block) = self.block_hashes.get(&maybe_head_block.parent_hash)
                    {
                        trace!(
                            child=%maybe_head_hash, parent=%parent_block.hash.unwrap(), "avoiding thundering herd",
                        );

                        maybe_head_block = parent_block.clone();
                        continue;
                    } else {
                        warn!(
                            "no parent to check. soft limit only {}/{} from {}/{} rpcs: {}%",
                            heavy_sum_soft_limit,
                            self.min_sum_soft_limit,
                            heavy_rpcs.len(),
                            self.min_synced_rpcs,
                            heavy_sum_soft_limit * 100 / self.min_sum_soft_limit
                        );
                        break;
                    }
                }

                // success! this block has enough soft limit and nodes on it (or on later blocks)
                let conns = heavy_rpcs.into_iter().cloned().collect();

                let heavy_block = maybe_head_block;

                let heavy_hash = heavy_block.hash.expect("head blocks always have hashes");
                let heavy_num = heavy_block.number.expect("head blocks always have numbers");

                debug_assert_ne!(heavy_num, U64::zero());

                let heavy_block_id = BlockId {
                    hash: heavy_hash,
                    num: heavy_num,
                };

                let new_synced_connections = SyncedConnections {
                    head_block_id: Some(heavy_block_id.clone()),
                    conns,
                };

                let old_synced_connections = self
                    .synced_connections
                    .swap(Arc::new(new_synced_connections));

                // TODO: add these to the log messages
                let num_connection_heads = connection_heads.len();
                let total_conns = self.conns.len();

                // TODO: if the rpc_head_block != heavy, log something somewhere in here
                match &old_synced_connections.head_block_id {
                    None => {
                        debug!(block=%heavy_block_id, %rpc, "first consensus head");

                        self.save_block(&rpc_head_block, Some(true))?;

                        head_block_sender.send(heavy_block)?;
                    }
                    Some(old_block_id) => {
                        match heavy_block_id.num.cmp(&old_block_id.num) {
                            Ordering::Equal => {
                                // multiple blocks with the same fork!
                                if heavy_block_id.hash == old_block_id.hash {
                                    // no change in hash. no need to use head_block_sender
                                    debug!(head=%heavy_block_id, old=%old_block_id, %rpc, "con block")
                                } else {
                                    // hash changed
                                    info!(heavy=%heavy_block_id, old=%old_block_id, %rpc, "unc block");

                                    // todo!("handle equal by updating the cannonical chain");
                                    self.save_block(&rpc_head_block, Some(true))?;

                                    head_block_sender.send(heavy_block)?;
                                }
                            }
                            Ordering::Less => {
                                // this is unlikely but possible
                                // TODO: better log
                                warn!(head=%heavy_block_id, %rpc, "chain rolled back");

                                self.save_block(&rpc_head_block, Some(true))?;

                                // todo!("handle less by removing higher blocks from the cannonical chain");
                                head_block_sender.send(heavy_block)?;
                            }
                            Ordering::Greater => {
                                debug!(head=%heavy_block_id, %rpc, "new block");

                                // todo!("handle greater by adding this block to and any missing parents to the cannonical chain");

                                self.save_block(&rpc_head_block, Some(true))?;

                                head_block_sender.send(heavy_block)?;
                            }
                        }
                    }
                }

                return Ok(());
            }

            // if we get here, something is wrong. clear synced connections
            let empty_synced_connections = SyncedConnections::default();

            let old_synced_connections = self
                .synced_connections
                .swap(Arc::new(empty_synced_connections));

            // TODO: log different things depending on old_synced_connections
        }

        Ok(())
    }
}
