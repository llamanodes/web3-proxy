///! Keep track of the blockchain as seen by a Web3Connections.
use super::connection::Web3Connection;
use super::connections::Web3Connections;
use super::synced_connections::SyncedConnections;
use crate::app::TxState;
use crate::jsonrpc::JsonRpcRequest;
use anyhow::Context;
use ethers::prelude::{Block, TxHash, H256, U256, U64};
use indexmap::IndexMap;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::{broadcast, watch};
use tracing::{debug, trace, warn};

pub(super) struct BlockMetadata<'a> {
    pub(super) block: &'a Arc<Block<TxHash>>,
    pub(super) sum_soft_limit: u32,
    pub(super) conns: Vec<&'a str>,
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
    pub fn add_block(&self, block: Arc<Block<TxHash>>, cannonical: bool) {
        let hash = block.hash.unwrap();

        if cannonical {
            let num = block.number.unwrap();

            let entry = self.chain_map.entry(num);

            let mut is_new = false;

            // TODO: this might be wrong. we might need to update parents, too
            entry.or_insert_with(|| {
                is_new = true;
                block.clone()
            });

            // TODO: prune chain_map?

            if !is_new {
                return;
            }
        }

        // TODO: prune block_map?

        self.block_map.entry(hash).or_insert(block);
    }

    pub async fn block(&self, hash: &H256) -> anyhow::Result<Arc<Block<TxHash>>> {
        // first, try to get the hash from our cache
        if let Some(block) = self.block_map.get(hash) {
            return Ok(block.clone());
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

        self.add_block(block.clone(), false);

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
    }

    pub async fn recv_block_from_rpc(
        &self,
        connection_heads: &mut IndexMap<String, Arc<Block<TxHash>>>,
        new_block: Arc<Block<TxHash>>,
        rpc: Arc<Web3Connection>,
        head_block_sender: &watch::Sender<Arc<Block<TxHash>>>,
        pending_tx_sender: &Option<broadcast::Sender<TxState>>,
    ) -> anyhow::Result<()> {
        let new_block_hash = if let Some(hash) = new_block.hash {
            hash
        } else {
            // TODO: rpc name instead of url (will do this with config reload revamp)
            connection_heads.remove(&rpc.name);

            // TODO: return here is wrong. synced rpcs needs an update
            return Ok(());
        };

        // TODO: dry this with the code above
        let new_block_num = if let Some(num) = new_block.number {
            num
        } else {
            // this seems unlikely, but i'm pretty sure we have seen it
            // maybe when a node is syncing or reconnecting?
            warn!(%rpc, ?new_block, "Block without number!");

            // TODO: rpc name instead of url (will do this with config reload revamp)
            connection_heads.remove(&rpc.name);

            // TODO: return here is wrong. synced rpcs needs an update
            return Ok(());
        };

        // TODO: span with more in it?
        // TODO: make sure i'm doing this span right
        // TODO: show the actual rpc url?
        // TODO: clippy lint to make sure we don't hold this across an awaited future
        // TODO: what level?
        // let _span = info_span!("block_receiver", %rpc, %new_block_num).entered();

        if new_block_num == U64::zero() {
            warn!(%rpc, %new_block_num, "still syncing");

            connection_heads.remove(&rpc.name);
        } else {
            connection_heads.insert(rpc.name.clone(), new_block.clone());

            self.add_block(new_block.clone(), false);
        }

        // iterate connection_heads to find the oldest block
        let lowest_block_num = if let Some(lowest_block) = connection_heads
            .values()
            .min_by(|a, b| a.number.cmp(&b.number))
        {
            lowest_block
                .number
                .expect("all blocks here should have a number")
        } else {
            // TODO: return here is wrong. synced rpcs needs an update
            return Ok(());
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
                total_soft_limit += rpc.soft_limit;

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
                    let rpc_urls_by_hash = rpcs_by_hash.entry(block_hash).or_insert_with(Vec::new);

                    rpc_urls_by_hash.push(rpc_url);

                    // save the rpcs by their number
                    let rpc_urls_by_num = rpcs_by_num.entry(block_num).or_insert_with(Vec::new);

                    rpc_urls_by_num.push(rpc_url);

                    if let Ok(parent) = self.block(&block.parent_hash).await {
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

        // TODO: default_min_soft_limit? without, we start serving traffic at the start too quickly
        // let min_soft_limit = total_soft_limit / 2;
        let min_soft_limit = 1;
        let num_possible_heads = rpcs_by_hash.len();

        trace!(?rpcs_by_hash);

        let total_rpcs = self.conns.len();

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

        Ok(())
    }
}
