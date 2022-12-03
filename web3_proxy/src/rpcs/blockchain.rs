///! Keep track of the blockchain as seen by a Web3Connections.
use super::connection::Web3Connection;
use super::connections::Web3Connections;
use super::transactions::TxStatus;
use crate::frontend::authorization::Authorization;
use crate::{
    config::BlockAndRpc, jsonrpc::JsonRpcRequest, rpcs::synced_connections::SyncedConnections,
};
use anyhow::Context;
use derive_more::From;
use ethers::prelude::{Block, TxHash, H256, U64};
use hashbrown::{HashMap, HashSet};
use log::{debug, warn, Level};
use moka::future::Cache;
use serde::Serialize;
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{cmp::Ordering, fmt::Display, sync::Arc};
use tokio::sync::{broadcast, watch};
use tokio::time::Duration;

// TODO: type for Hydrated Blocks with their full transactions?
pub type ArcBlock = Arc<Block<TxHash>>;

pub type BlockHashesCache = Cache<H256, ArcBlock, hashbrown::hash_map::DefaultHashBuilder>;

/// A block's hash and number.
#[derive(Clone, Debug, Default, From, Serialize)]
pub struct SavedBlock {
    pub block: ArcBlock,
    /// number of seconds this block was behind the current time when received
    lag: u64,
}

impl SavedBlock {
    pub fn new(block: ArcBlock) -> Self {
        // TODO: read this from a global config. different chains should probably have different gaps.
        let allowed_lag: u64 = 60;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("there should always be time");

        // TODO: get this from config
        // TODO: is this safe enough? what if something about the chain is actually lagged? what if its a chain like BTC with 10 minute blocks?
        let oldest_allowed = now - Duration::from_secs(allowed_lag);

        let block_timestamp = Duration::from_secs(block.timestamp.as_u64());

        // TODO: recalculate this every time?
        let lag = if block_timestamp < oldest_allowed {
            // this server is still syncing from too far away to serve requests
            // u64 is safe because ew checked equality above
            (oldest_allowed - block_timestamp).as_secs() as u64
        } else {
            0
        };

        Self { block, lag }
    }

    pub fn hash(&self) -> H256 {
        self.block.hash.unwrap()
    }

    // TODO: return as U64 or u64?
    pub fn number(&self) -> U64 {
        self.block.number.unwrap()
    }

    /// When the block was received, this node was still syncing
    pub fn syncing(&self) -> bool {
        // TODO: margin should come from a global config
        self.lag > 60
    }
}

impl From<ArcBlock> for SavedBlock {
    fn from(x: ArcBlock) -> Self {
        SavedBlock::new(x)
    }
}

impl Display for SavedBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.number(), self.hash())?;

        if self.syncing() {
            write!(f, " (behind by {} seconds)", self.lag)?;
        }

        Ok(())
    }
}

impl Web3Connections {
    /// add a block to our mappings and track the heaviest chain
    pub async fn save_block(&self, block: &ArcBlock, heaviest_chain: bool) -> anyhow::Result<()> {
        // TODO: i think we can rearrange this function to make it faster on the hot path
        let block_hash = block.hash.as_ref().context("no block hash")?;

        // skip Block::default()
        if block_hash.is_zero() {
            debug!("Skipping block without hash!");
            return Ok(());
        }

        let block_num = block.number.as_ref().context("no block num")?;

        // TODO: think more about heaviest_chain. would be better to do the check inside this function
        if heaviest_chain {
            // this is the only place that writes to block_numbers
            // multiple inserts should be okay though
            // TODO: info that there was a fork?
            self.block_numbers.insert(*block_num, *block_hash).await;
        }

        // this block is very likely already in block_hashes
        // TODO: use their get_with
        self.block_hashes
            .get_with(*block_hash, async move { block.clone() })
            .await;

        Ok(())
    }

    /// Get a block from caches with fallback.
    /// Will query a specific node or the best available.
    pub async fn block(
        &self,
        authorization: &Arc<Authorization>,
        hash: &H256,
        rpc: Option<&Arc<Web3Connection>>,
    ) -> anyhow::Result<ArcBlock> {
        // first, try to get the hash from our cache
        // the cache is set last, so if its here, its everywhere
        if let Some(block) = self.block_hashes.get(hash) {
            return Ok(block);
        }

        // block not in cache. we need to ask an rpc for it
        let get_block_params = (*hash, false);
        // TODO: if error, retry?
        let block: ArcBlock = match rpc {
            Some(rpc) => {
                rpc.wait_for_request_handle(authorization, Duration::from_secs(30))
                    .await?
                    .request(
                        "eth_getBlockByHash",
                        &json!(get_block_params),
                        Level::Error.into(),
                    )
                    .await?
            }
            None => {
                // TODO: helper for method+params => JsonRpcRequest
                // TODO: does this id matter?
                let request = json!({ "id": "1", "method": "eth_getBlockByHash", "params": get_block_params });
                let request: JsonRpcRequest = serde_json::from_value(request)?;

                // TODO: request_metadata? maybe we should put it in the authorization?
                let response = self
                    .try_send_best_upstream_server(authorization, request, None, None)
                    .await?;

                let block = response.result.context("failed fetching block")?;

                serde_json::from_str(block.get())?
            }
        };

        // the block was fetched using eth_getBlockByHash, so it should have all fields
        // TODO: fill in heaviest_chain! if the block is old enough, is this definitely true?
        self.save_block(&block, false).await?;

        Ok(block)
    }

    /// Convenience method to get the cannonical block at a given block height.
    pub async fn block_hash(
        &self,
        authorization: &Arc<Authorization>,
        num: &U64,
    ) -> anyhow::Result<(H256, bool)> {
        let (block, is_archive_block) = self.cannonical_block(authorization, num).await?;

        let hash = block.hash.expect("Saved blocks should always have hashes");

        Ok((hash, is_archive_block))
    }

    /// Get the heaviest chain's block from cache or backend rpc
    pub async fn cannonical_block(
        &self,
        authorization: &Arc<Authorization>,
        num: &U64,
    ) -> anyhow::Result<(ArcBlock, bool)> {
        // we only have blocks by hash now
        // maybe save them during save_block in a blocks_by_number Cache<U64, Vec<ArcBlock>>
        // if theres multiple, use petgraph to find the one on the main chain (and remove the others if they have enough confirmations)

        // be sure the requested block num exists
        let head_block_num = self.head_block_num().context("no servers in sync")?;

        // TODO: geth does 64, erigon does 90k. sometimes we run a mix
        let archive_needed = num < &(head_block_num - U64::from(64));

        if num > &head_block_num {
            // TODO: i'm seeing this a lot when using ethspam. i dont know why though. i thought we delayed publishing
            // TODO: instead of error, maybe just sleep and try again?
            // TODO: this should be a 401, not a 500
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
            // TODO: pass authorization through here?
            let block = self.block(authorization, &block_hash, None).await?;

            return Ok((block, archive_needed));
        }

        // block number not in cache. we need to ask an rpc for it
        // TODO: helper for method+params => JsonRpcRequest
        let request = json!({ "jsonrpc": "2.0", "id": "1", "method": "eth_getBlockByNumber", "params": (num, false) });
        let request: JsonRpcRequest = serde_json::from_value(request)?;

        // TODO: if error, retry?
        // TODO: request_metadata or authorization?
        let response = self
            .try_send_best_upstream_server(authorization, request, None, Some(num))
            .await?;

        let raw_block = response.result.context("no block result")?;

        let block: ArcBlock = serde_json::from_str(raw_block.get())?;

        // the block was fetched using eth_getBlockByNumber, so it should have all fields and be on the heaviest chain
        self.save_block(&block, true).await?;

        Ok((block, archive_needed))
    }

    pub(super) async fn process_incoming_blocks(
        &self,
        authorization: &Arc<Authorization>,
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
            let new_block = new_block.map(Into::into);

            let rpc_name = rpc.name.clone();

            if let Err(err) = self
                .process_block_from_rpc(
                    authorization,
                    &mut connection_heads,
                    new_block,
                    rpc,
                    &head_block_sender,
                    &pending_tx_sender,
                )
                .await
            {
                warn!("unable to process block from rpc {}: {:?}", rpc_name, err);
            }
        }

        // TODO: if there was an error, we should return it
        warn!("block_receiver exited!");

        Ok(())
    }

    /// `connection_heads` is a mapping of rpc_names to head block hashes.
    /// self.blockchain_map is a mapping of hashes to the complete ArcBlock.
    /// TODO: return something?
    pub(crate) async fn process_block_from_rpc(
        &self,
        authorization: &Arc<Authorization>,
        connection_heads: &mut HashMap<String, H256>,
        rpc_head_block: Option<SavedBlock>,
        rpc: Arc<Web3Connection>,
        head_block_sender: &watch::Sender<ArcBlock>,
        pending_tx_sender: &Option<broadcast::Sender<TxStatus>>,
    ) -> anyhow::Result<()> {
        // add the rpc's block to connection_heads, or remove the rpc from connection_heads
        let rpc_head_block = match rpc_head_block {
            Some(rpc_head_block) => {
                let rpc_head_num = rpc_head_block.number();
                let rpc_head_hash = rpc_head_block.hash();

                // we don't know if its on the heaviest chain yet
                self.save_block(&rpc_head_block.block, false).await?;

                if rpc_head_block.syncing() {
                    if connection_heads.remove(&rpc.name).is_some() {
                        warn!("{} is behind by {} seconds", &rpc.name, rpc_head_block.lag);
                    };

                    None
                } else {
                    if let Some(prev_hash) =
                        connection_heads.insert(rpc.name.to_owned(), rpc_head_hash)
                    {
                        if prev_hash == rpc_head_hash {
                            // this block was already sent by this node. return early
                            return Ok(());
                        }
                    }

                    // TODO: should we just keep the ArcBlock here?
                    Some(rpc_head_block)
                }
            }
            None => {
                // TODO: warn is too verbose. this is expected if a node disconnects and has to reconnect
                // // trace!(%rpc, "Block without number or hash!");

                if connection_heads.remove(&rpc.name).is_none() {
                    // this connection was already removed.
                    // return early. no need to process synced connections
                    return Ok(());
                }

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
                warn!("Missing connection_head_block in block_hashes. Fetching now. hash={}. other={}. rpc={}", connection_head_hash, conn_name, rpc);

                // this option should always be populated
                let conn_rpc = self.conns.get(conn_name);

                match self
                    .block(authorization, connection_head_hash, conn_rpc)
                    .await
                {
                    Ok(block) => block,
                    Err(err) => {
                        warn!("Processing {}. Failed fetching connection_head_block for block_hashes. {} head hash={}. err={:?}", rpc, conn_name, connection_head_hash, err);
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
                    || highest_rpcs.len() < self.min_head_rpcs
                {
                    // not enough rpcs yet. check the parent
                    if let Some(parent_block) = self.block_hashes.get(&maybe_head_block.parent_hash)
                    {
                        // // trace!(
                        //     child=%maybe_head_hash, parent=%parent_block.hash.unwrap(), "avoiding thundering herd",
                        // );

                        maybe_head_block = parent_block;
                        continue;
                    } else {
                        warn!(
                            "no parent to check. soft limit only {}/{} from {}/{} rpcs: {}%",
                            highest_rpcs_sum_soft_limit,
                            self.min_sum_soft_limit,
                            highest_rpcs.len(),
                            self.min_head_rpcs,
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

                let _ = self
                    .synced_connections
                    .swap(Arc::new(empty_synced_connections));

                // TODO: log different things depending on old_synced_connections
                warn!(
                    "Processing {}. no consensus head! {}/{}/{}",
                    rpc, 0, num_connection_heads, total_conns
                );
            } else {
                // // trace!(?highest_rpcs);

                // TODO: if maybe_head_block.time() is old, ignore it

                // success! this block has enough soft limit and nodes on it (or on later blocks)
                let conns: Vec<Arc<Web3Connection>> = highest_rpcs
                    .into_iter()
                    .filter_map(|conn_name| self.conns.get(conn_name).cloned())
                    .collect();

                let consensus_head_hash = maybe_head_block
                    .hash
                    .expect("head blocks always have hashes");
                let consensus_head_num = maybe_head_block
                    .number
                    .expect("head blocks always have numbers");

                let num_consensus_rpcs = conns.len();

                let consensus_head_block: SavedBlock = maybe_head_block.into();

                let new_synced_connections = SyncedConnections {
                    head_block_id: Some(consensus_head_block.clone()),
                    conns,
                };

                let old_synced_connections = self
                    .synced_connections
                    .swap(Arc::new(new_synced_connections));

                // TODO: if the rpc_head_block != consensus_head_block, log something?
                match &old_synced_connections.head_block_id {
                    None => {
                        debug!(
                            "first {}/{}/{} block={}, rpc={}",
                            num_consensus_rpcs,
                            num_connection_heads,
                            total_conns,
                            consensus_head_block,
                            rpc
                        );

                        self.save_block(&consensus_head_block.block, true).await?;

                        head_block_sender
                            .send(consensus_head_block.block)
                            .context("head_block_sender sending consensus_head_block")?;
                    }
                    Some(old_head_block) => {
                        // TODO: do this log item better
                        let rpc_head_str = rpc_head_block
                            .map(|x| x.to_string())
                            .unwrap_or_else(|| "None".to_string());

                        match consensus_head_block.number().cmp(&old_head_block.number()) {
                            Ordering::Equal => {
                                // TODO: if rpc_block_id != consensus_head_block_id, do a different log?

                                // multiple blocks with the same fork!
                                if consensus_head_block.hash() == old_head_block.hash() {
                                    // no change in hash. no need to use head_block_sender
                                    debug!(
                                        "con {}/{}/{} con_head={} rpc={} rpc_head={}",
                                        num_consensus_rpcs,
                                        num_connection_heads,
                                        total_conns,
                                        consensus_head_block,
                                        rpc,
                                        rpc_head_str
                                    )
                                } else {
                                    // hash changed
                                    debug!(
                                        "unc {}/{}/{} con_head={} old={} rpc_head={} rpc={}",
                                        num_consensus_rpcs,
                                        num_connection_heads,
                                        total_conns,
                                        consensus_head_block,
                                        old_head_block,
                                        rpc_head_str,
                                        rpc
                                    );

                                    self.save_block(&consensus_head_block.block, true)
                                        .await
                                        .context("save consensus_head_block as heaviest chain")?;

                                    head_block_sender.send(consensus_head_block.block).context(
                                        "head_block_sender sending consensus_head_block",
                                    )?;
                                }
                            }
                            Ordering::Less => {
                                // this is unlikely but possible
                                // TODO: better log
                                warn!("chain rolled back {}/{}/{} con_head={} old_head={} rpc_head={} rpc={}", num_consensus_rpcs, num_connection_heads, total_conns, consensus_head_block, old_head_block, rpc_head_str, rpc);

                                // TODO: tell save_block to remove any higher block numbers from the cache. not needed because we have other checks on requested blocks being > head, but still seems slike a good idea
                                self.save_block(&consensus_head_block.block, true)
                                    .await
                                    .context(
                                        "save_block sending consensus_head_block as heaviest chain",
                                    )?;

                                head_block_sender
                                    .send(consensus_head_block.block)
                                    .context("head_block_sender sending consensus_head_block")?;
                            }
                            Ordering::Greater => {
                                debug!(
                                    "new {}/{}/{} con_head={} rpc_head={} rpc={}",
                                    num_consensus_rpcs,
                                    num_connection_heads,
                                    total_conns,
                                    consensus_head_block,
                                    rpc_head_str,
                                    rpc
                                );

                                self.save_block(&consensus_head_block.block, true).await?;

                                head_block_sender.send(consensus_head_block.block)?;
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }
}
