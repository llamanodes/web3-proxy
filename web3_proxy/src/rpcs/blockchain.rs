///! Keep track of the blockchain as seen by a Web3Connections.
use super::connection::Web3Connection;
use super::connections::Web3Connections;
use super::transactions::TxStatus;
use crate::{config::BlockAndRpc, jsonrpc::JsonRpcRequest};
use derive_more::From;
use ethers::prelude::{Block, TxHash, H256, U256, U64};
use hashbrown::HashMap;
use petgraph::prelude::DiGraphMap;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::{broadcast, watch};
use tracing::{debug, warn};

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

/// TODO: do we need this? probably big refactor still to do
/// The RPCs grouped by number and hash.
#[derive(Default)]
struct BlockchainAndRpcs<'a> {
    // TODO: fifomap? or just manually remove once we add too much
    rpcs_by_num: HashMap<U64, Vec<&'a str>>,
    rpcs_by_hash: HashMap<H256, Vec<&'a str>>,
    blocks_by_hash: HashMap<H256, Arc<Block<TxHash>>>,
    /// Node is the blockhash.
    /// You can get the blocks from block_map on the Web3Connections
    /// TODO: what should the edge weight be? difficulty?
    blockchain: DiGraphMap<H256, u8>,
    total_soft_limit: u32,
}

impl<'a> BlockchainAndRpcs<'a> {
    /// group the RPCs by their current head block
    pub async fn new(
        // TODO: think more about this key. maybe it should be an Arc?
        connection_heads: &'a HashMap<String, Arc<Block<TxHash>>>,
        web3_conns: &Web3Connections,
    ) -> Option<BlockchainAndRpcs<'a>> {
        let mut new = Self::default();

        let lowest_block_num = if let Some(lowest_block) = connection_heads
            .values()
            .min_by(|a, b| a.number.cmp(&b.number))
        {
            lowest_block
                .number
                .expect("all blocks here should have a number")
        } else {
            // if no lowest block number, then no servers are in sync
            return None;
        };

        // TODO: what if lowest_block_num is far from the highest head block num?

        for (rpc_name, head_block) in connection_heads.iter() {
            if let Some(rpc) = web3_conns.get(rpc_name) {
                // we need the total soft limit in order to know when its safe to update the backends
                new.total_soft_limit += rpc.soft_limit;

                let head_hash = head_block.hash.unwrap();

                // save the block
                new.blocks_by_hash
                    .entry(head_hash)
                    .or_insert_with(|| head_block.clone());

                // add the rpc to all relevant block heights
                // TODO: i feel like we should be able to do this with a graph
                let mut block = head_block.clone();
                while block.number.unwrap() >= lowest_block_num {
                    let block_hash = block.hash.unwrap();
                    let block_num = block.number.unwrap();

                    // save the rpc by the head hash
                    let rpc_urls_by_hash =
                        new.rpcs_by_hash.entry(block_hash).or_insert_with(Vec::new);
                    rpc_urls_by_hash.push(rpc_name);

                    // save the rpc by the head number
                    let rpc_names_by_num =
                        new.rpcs_by_num.entry(block_num).or_insert_with(Vec::new);
                    rpc_names_by_num.push(rpc_name);

                    if let Ok(parent) = web3_conns
                        .block(&block.parent_hash, Some(rpc.as_ref()))
                        .await
                    {
                        // save the parent block
                        new.blocks_by_hash.insert(block.parent_hash, parent.clone());

                        block = parent
                    } else {
                        // log this? eventually we will hit a block we don't have, so it's not an error
                        break;
                    }
                }
            }
        }

        Some(new)
    }

    fn consensus_head() {
        todo!()
    }
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
    pub fn add_block_to_chain(&self, block: Arc<Block<TxHash>>) -> anyhow::Result<()> {
        let hash = block.hash.ok_or_else(|| anyhow::anyhow!("no block hash"))?;

        if self.blockchain_map.read().contains_node(hash) {
            // this block is already included
            return Ok(());
        }

        // theres a small race having the read and then the write
        let mut blockchain = self.blockchain_map.write();

        if blockchain.contains_node(hash) {
            // this hash is already included. we must have hit that race condition
            // return now since this work was already done.
            return Ok(());
        }

        // TODO: prettier log? or probably move the log somewhere else
        debug!(%hash, "new block");

        // TODO: prune block_map to only keep a configurable (256 on ETH?) number of blocks?

        blockchain.add_node(hash);

        // what should edge weight be? and should the nodes be the blocks instead?
        // maybe the weight should be the height
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

        self.add_block_to_chain(block.clone())?;

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
            self.recv_block_from_rpc(
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
    ///
    async fn recv_block_from_rpc(
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

                    self.add_block_to_chain(new_block.clone())?;
                }
            }
            _ => {
                warn!(%rpc, ?new_block, "Block without number or hash!");

                connection_heads.remove(&rpc.name);

                // don't return yet! self.synced_connections likely needs an update
            }
        }

        let mut chain_and_rpcs = BlockchainAndRpcs::default();

        // TODO: default_min_soft_limit? without, we start serving traffic at the start too quickly
        // let min_soft_limit = total_soft_limit / 2;
        let min_soft_limit = 1;

        let num_possible_heads = chain_and_rpcs.rpcs_by_hash.len();

        // trace!(?rpcs_by_hash);

        let total_rpcs = self.conns.len();

        /*
        // TODO: this needs tests
        if let Some(x) = rpcs_by_hash
            .into_iter()
            .filter_map(|(hash, conns)| {
                // TODO: move this to `State::new` function on
                let sum_soft_limit = conns
                    .iter()
                    .map(|rpc_url| {
                        if let Some(rpc) = self.conns.get(*rpc_url) {
                            rpc.soft_limit
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

                    Some(BlockMetadata {
                        block,
                        sum_soft_limit,
                        conns,
                    })
                }
            })
            // sort b to a for descending order. sort a to b for ascending order? maybe not "max_by" is smart
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
                // TODO: this isn't necessarily a fork. this might just be an rpc being slow
                // TODO: log all the heads?
                warn!(
                    "chain is forked! {} possible heads. {}/{}/{}/{} rpcs have {}",
                    num_possible_heads,
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
                head_block_num: best_head_num,
                head_block_hash: best_head_hash,
                conns,
            };

            let current_head_block = self.head_block_hash();
            let new_head_block = pending_synced_connections.head_block_hash != current_head_block;

            if new_head_block {
                self.add_block(new_block.clone(), true);

                debug!(
                    "{}/{} rpcs at {} ({}). head at {:?}",
                    pending_synced_connections.conns.len(),
                    self.conns.len(),
                    pending_synced_connections.head_block_num,
                    pending_synced_connections.head_block_hash,
                    pending_synced_connections
                        .conns
                        .iter()
                        .map(|x| format!("{}", x))
                        .collect::<Vec<_>>(),
                );
                // TODO: what if the hashes don't match?
                if Some(pending_synced_connections.head_block_hash) == new_block.hash {
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
            } else if num_best_rpcs == self.conns.len() {
                trace!(
                    "all {} rpcs at {} ({})",
                    num_best_rpcs,
                    pending_synced_connections.head_block_num,
                    pending_synced_connections.head_block_hash,
                );
            } else {
                trace!(
                    ?pending_synced_connections,
                    "{}/{} rpcs at {} ({})",
                    num_best_rpcs,
                    self.conns.len(),
                    pending_synced_connections.head_block_num,
                    pending_synced_connections.head_block_hash,
                );
            }

            // TODO: do this before or after processing all the transactions in this block?
            // TODO: only swap if there is a change?
            trace!(?pending_synced_connections, "swapping");
            self.synced_connections
                .swap(Arc::new(pending_synced_connections));

            if new_head_block {
                // TODO: is new_head_block accurate?
                // TODO: move this onto self.chain?
                head_block_sender
                    .send(new_block.clone())
                    .context("head_block_sender")?;
            }
        } else {
            // TODO: is this expected when we first start?
            // TODO: make sure self.synced_connections is empty
            // TODO: return an error
            warn!("not enough rpcs in sync");
        }

        */
        Ok(())
    }
}
