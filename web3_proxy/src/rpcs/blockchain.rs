///! Keep track of the blockchain as seen by a Web3Connections.
use super::connection::Web3Connection;
use super::connections::Web3Connections;
use super::transactions::TxStatus;
use crate::frontend::authorization::Authorization;
use crate::{
    config::BlockAndRpc, jsonrpc::JsonRpcRequest, rpcs::synced_connections::ConsensusConnections,
};
use anyhow::Context;
use derive_more::From;
use ethers::prelude::{Block, TxHash, H256, U64};
use hashbrown::{HashMap, HashSet};
use tracing::{debug, error, instrument, warn, Level};
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

/// A block and its age.
#[derive(Clone, Debug, Default, From, Serialize)]
pub struct SavedBlock {
    pub block: ArcBlock,
    /// number of seconds this block was behind the current time when received
    pub age: u64,
}

impl PartialEq for SavedBlock {
    #[instrument(level = "trace")]
    fn eq(&self, other: &Self) -> bool {
        match (self.block.hash, other.block.hash) {
            (None, None) => true,
            (Some(_), None) => false,
            (None, Some(_)) => false,
            (Some(s), Some(o)) => s == o,
        }
    }
}

impl SavedBlock {
    #[instrument(level = "trace")]
    pub fn new(block: ArcBlock) -> Self {
        let mut x = Self { block, age: 0 };

        // no need to recalulate lag every time
        // if the head block gets too old, a health check restarts this connection
        x.age = x.lag();

        x
    }

    #[instrument(level = "trace")]
    pub fn lag(&self) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("there should always be time");

        let block_timestamp = Duration::from_secs(self.block.timestamp.as_u64());

        if block_timestamp < now {
            // this server is still syncing from too far away to serve requests
            // u64 is safe because ew checked equality above
            (now - block_timestamp).as_secs() as u64
        } else {
            0
        }
    }

    #[instrument(level = "trace")]
    pub fn hash(&self) -> H256 {
        self.block.hash.expect("saved blocks must have a hash")
    }

    // TODO: return as U64 or u64?
    #[instrument(level = "trace")]
    pub fn number(&self) -> U64 {
        self.block.number.expect("saved blocks must have a number")
    }
}

impl From<ArcBlock> for SavedBlock {
    #[instrument(level = "trace")]
    fn from(x: ArcBlock) -> Self {
        SavedBlock::new(x)
    }
}

impl Display for SavedBlock {
    #[instrument(skip_all)]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}, {}s old)", self.number(), self.hash(), self.age)
    }
}

impl Web3Connections {
    /// add a block to our mappings and track the heaviest chain
    #[instrument(level = "trace")]
    pub async fn save_block(
        &self,
        block: ArcBlock,
        heaviest_chain: bool,
    ) -> anyhow::Result<ArcBlock> {
        // TODO: i think we can rearrange this function to make it faster on the hot path
        let block_hash = block.hash.as_ref().context("no block hash")?;

        // skip Block::default()
        if block_hash.is_zero() {
            debug!("Skipping block without hash!");
            return Ok(block);
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
        let block = self
            .block_hashes
            .get_with(*block_hash, async move { block.clone() })
            .await;

        Ok(block)
    }

    /// Get a block from caches with fallback.
    /// Will query a specific node or the best available.
    /// TODO: return anyhow::Result<Option<ArcBlock>>?
    #[instrument(level = "trace")]
    pub async fn block(
        &self,
        authorization: &Arc<Authorization>,
        hash: &H256,
        rpc: Option<&Arc<Web3Connection>>,
    ) -> anyhow::Result<ArcBlock> {
        // first, try to get the hash from our cache
        // the cache is set last, so if its here, its everywhere
        // TODO: use try_get_with
        if let Some(block) = self.block_hashes.get(hash) {
            return Ok(block);
        }

        // block not in cache. we need to ask an rpc for it
        let get_block_params = (*hash, false);
        // TODO: if error, retry?
        let block: ArcBlock = match rpc {
            Some(rpc) => rpc
                .wait_for_request_handle(authorization, Some(Duration::from_secs(30)), false)
                .await?
                .request::<_, Option<_>>(
                    "eth_getBlockByHash",
                    &json!(get_block_params),
                    Level::Error.into(),
                )
                .await?
                .context("no block!")?,
            None => {
                // TODO: helper for method+params => JsonRpcRequest
                // TODO: does this id matter?
                let request = json!({ "id": "1", "method": "eth_getBlockByHash", "params": get_block_params });
                let request: JsonRpcRequest = serde_json::from_value(request)?;

                // TODO: request_metadata? maybe we should put it in the authorization?
                // TODO: think more about this wait_for_sync
                let response = self
                    .try_send_best_consensus_head_connection(authorization, request, None, None)
                    .await?;

                let block = response.result.context("failed fetching block")?;

                let block: Option<ArcBlock> = serde_json::from_str(block.get())?;

                block.context("no block!")?
            }
        };

        // the block was fetched using eth_getBlockByHash, so it should have all fields
        // TODO: fill in heaviest_chain! if the block is old enough, is this definitely true?
        let block = self.save_block(block, false).await?;

        Ok(block)
    }

    /// Convenience method to get the cannonical block at a given block height.
    #[instrument(level = "trace")]
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
    /// Caution! If a future block is requested, this might wait forever. Be sure to have a timeout outside of this!
    #[instrument(level = "trace")]
    pub async fn cannonical_block(
        &self,
        authorization: &Arc<Authorization>,
        num: &U64,
    ) -> anyhow::Result<(ArcBlock, bool)> {
        // we only have blocks by hash now
        // maybe save them during save_block in a blocks_by_number Cache<U64, Vec<ArcBlock>>
        // if theres multiple, use petgraph to find the one on the main chain (and remove the others if they have enough confirmations)

        let mut consensus_head_receiver = self
            .watch_consensus_head_receiver
            .as_ref()
            .context("need new head subscriptions to fetch cannonical_block")?
            .clone();

        // be sure the requested block num exists
        let mut head_block_num = consensus_head_receiver.borrow_and_update().number;

        loop {
            if let Some(head_block_num) = head_block_num {
                if num <= &head_block_num {
                    break;
                }
            }

            consensus_head_receiver.changed().await?;

            head_block_num = consensus_head_receiver.borrow_and_update().number;
        }

        let head_block_num =
            head_block_num.expect("we should only get here if we have a head block");

        // TODO: geth does 64, erigon does 90k. sometimes we run a mix
        let archive_needed = num < &(head_block_num - U64::from(64));

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
        // we don't actually set min_block_needed here because all nodes have all blocks
        let response = self
            .try_send_best_consensus_head_connection(authorization, request, None, None)
            .await?;

        if let Some(err) = response.error {
            debug!("could not find canonical block {}: {:?}", num, err);
        }

        let raw_block = response.result.context("no cannonical block result")?;

        let block: ArcBlock = serde_json::from_str(raw_block.get())?;

        // the block was fetched using eth_getBlockByNumber, so it should have all fields and be on the heaviest chain
        let block = self.save_block(block, true).await?;

        Ok((block, archive_needed))
    }

    #[instrument(level = "trace")]
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
        let mut connection_heads = ConsensusFinder::default();

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

        // TODO: if there was an error, should we return it instead of an Ok?
        warn!("block_receiver exited!");

        Ok(())
    }

    /// `connection_heads` is a mapping of rpc_names to head block hashes.
    /// self.blockchain_map is a mapping of hashes to the complete ArcBlock.
    /// TODO: return something?
    #[instrument(level = "trace")]
    pub(crate) async fn process_block_from_rpc(
        &self,
        authorization: &Arc<Authorization>,
        consensus_finder: &mut ConsensusFinder,
        rpc_head_block: Option<SavedBlock>,
        rpc: Arc<Web3Connection>,
        head_block_sender: &watch::Sender<ArcBlock>,
        pending_tx_sender: &Option<broadcast::Sender<TxStatus>>,
    ) -> anyhow::Result<()> {
        // TODO: how should we handle an error here?
        if !consensus_finder
            .update_rpc(rpc_head_block.clone(), rpc.clone(), self)
            .await?
        {
            // nothing changed. no need
            return Ok(());
        }

        let new_synced_connections = consensus_finder
            .best_consensus_connections(authorization, self)
            .await;

        // TODO: what should we do if the block number of new_synced_connections is < old_synced_connections? wait?

        let includes_backups = new_synced_connections.includes_backups;
        let consensus_head_block = new_synced_connections.head_block.clone();
        let num_consensus_rpcs = new_synced_connections.num_conns();
        let num_checked_rpcs = new_synced_connections.num_checked_conns;
        let num_active_rpcs = consensus_finder.all.rpc_name_to_hash.len();
        let total_rpcs = self.conns.len();

        let old_consensus_head_connections = self
            .watch_consensus_connections_sender
            .send_replace(Arc::new(new_synced_connections));

        let includes_backups_str = if includes_backups { "B " } else { "" };

        if let Some(consensus_saved_block) = consensus_head_block {
            match &old_consensus_head_connections.head_block {
                None => {
                    debug!(
                        "first {}{}/{}/{}/{} block={}, rpc={}",
                        includes_backups_str,
                        num_consensus_rpcs,
                        num_checked_rpcs,
                        num_active_rpcs,
                        total_rpcs,
                        consensus_saved_block,
                        rpc,
                    );

                    if includes_backups {
                        // TODO: what else should be in this error?
                        warn!("Backup RPCs are in use!");
                    }

                    let consensus_head_block =
                        self.save_block(consensus_saved_block.block, true).await?;

                    head_block_sender
                        .send(consensus_head_block)
                        .context("head_block_sender sending consensus_head_block")?;
                }
                Some(old_head_block) => {
                    // TODO: do this log item better
                    let rpc_head_str = rpc_head_block
                        .map(|x| x.to_string())
                        .unwrap_or_else(|| "None".to_string());

                    match consensus_saved_block.number().cmp(&old_head_block.number()) {
                        Ordering::Equal => {
                            // multiple blocks with the same fork!
                            if consensus_saved_block.hash() == old_head_block.hash() {
                                // no change in hash. no need to use head_block_sender
                                debug!(
                                    "con {}{}/{}/{}/{} con={} rpc={}@{}",
                                    includes_backups_str,
                                    num_consensus_rpcs,
                                    num_checked_rpcs,
                                    num_active_rpcs,
                                    total_rpcs,
                                    consensus_saved_block,
                                    rpc,
                                    rpc_head_str,
                                )
                            } else {
                                // hash changed

                                if includes_backups {
                                    // TODO: what else should be in this error?
                                    warn!("Backup RPCs are in use!");
                                }

                                debug!(
                                    "unc {}{}/{}/{}/{} con_head={} old={} rpc={}@{}",
                                    includes_backups_str,
                                    num_consensus_rpcs,
                                    num_checked_rpcs,
                                    num_active_rpcs,
                                    total_rpcs,
                                    consensus_saved_block,
                                    old_head_block,
                                    rpc,
                                    rpc_head_str,
                                );

                                let consensus_head_block = self
                                    .save_block(consensus_saved_block.block, true)
                                    .await
                                    .context("save consensus_head_block as heaviest chain")?;

                                head_block_sender
                                    .send(consensus_head_block)
                                    .context("head_block_sender sending consensus_head_block")?;
                            }
                        }
                        Ordering::Less => {
                            // this is unlikely but possible
                            // TODO: better log
                            warn!(
                                "chain rolled back {}{}/{}/{}/{} con={} old={} rpc={}@{}",
                                includes_backups_str,
                                num_consensus_rpcs,
                                num_checked_rpcs,
                                num_active_rpcs,
                                total_rpcs,
                                consensus_saved_block,
                                old_head_block,
                                rpc,
                                rpc_head_str,
                            );

                            if includes_backups {
                                // TODO: what else should be in this error?
                                warn!("Backup RPCs are in use!");
                            }

                            // TODO: tell save_block to remove any higher block numbers from the cache. not needed because we have other checks on requested blocks being > head, but still seems like a good idea
                            let consensus_head_block = self
                                .save_block(consensus_saved_block.block, true)
                                .await
                                .context(
                                    "save_block sending consensus_head_block as heaviest chain",
                                )?;

                            head_block_sender
                                .send(consensus_head_block)
                                .context("head_block_sender sending consensus_head_block")?;
                        }
                        Ordering::Greater => {
                            debug!(
                                "new {}{}/{}/{}/{} con={} rpc={}@{}",
                                includes_backups_str,
                                num_consensus_rpcs,
                                num_checked_rpcs,
                                num_active_rpcs,
                                total_rpcs,
                                consensus_saved_block,
                                rpc,
                                rpc_head_str,
                            );

                            if includes_backups {
                                // TODO: what else should be in this error?
                                warn!("Backup RPCs are in use!");
                            }

                            let consensus_head_block =
                                self.save_block(consensus_saved_block.block, true).await?;

                            head_block_sender.send(consensus_head_block)?;
                        }
                    }
                }
            }
        } else {
            // TODO: do this log item better
            let rpc_head_str = rpc_head_block
                .map(|x| x.to_string())
                .unwrap_or_else(|| "None".to_string());

            if num_checked_rpcs >= self.min_head_rpcs {
                error!(
                    "non {}{}/{}/{}/{} rpc={}@{}",
                    includes_backups_str,
                    num_consensus_rpcs,
                    num_checked_rpcs,
                    num_active_rpcs,
                    total_rpcs,
                    rpc,
                    rpc_head_str,
                );
            } else {
                debug!(
                    "non {}{}/{}/{}/{} rpc={}@{}",
                    includes_backups_str,
                    num_consensus_rpcs,
                    num_checked_rpcs,
                    num_active_rpcs,
                    total_rpcs,
                    rpc,
                    rpc_head_str,
                );
            }
        }

        Ok(())
    }
}

#[derive(Debug)]
struct ConnectionsGroup {
    /// TODO: this group might not actually include backups, but they were at leastchecked
    includes_backups: bool,
    rpc_name_to_hash: HashMap<String, H256>,
}

impl ConnectionsGroup {
    #[instrument(level = "trace")]
    fn new(with_backups: bool) -> Self {
        Self {
            includes_backups: with_backups,
            rpc_name_to_hash: Default::default(),
        }
    }

    #[instrument(level = "trace")]
    fn without_backups() -> Self {
        Self::new(false)
    }

    #[instrument(level = "trace")]
    fn with_backups() -> Self {
        Self::new(true)
    }

    #[instrument(level = "trace")]
    fn remove(&mut self, rpc: &Web3Connection) -> Option<H256> {
        self.rpc_name_to_hash.remove(rpc.name.as_str())
    }

    #[instrument(level = "trace")]
    fn insert(&mut self, rpc: &Web3Connection, block_hash: H256) -> Option<H256> {
        self.rpc_name_to_hash.insert(rpc.name.clone(), block_hash)
    }

    // TODO: i don't love having this here. move to web3_connections?
    #[instrument(level = "trace")]
    async fn get_block_from_rpc(
        &self,
        rpc_name: &str,
        hash: &H256,
        authorization: &Arc<Authorization>,
        web3_connections: &Web3Connections,
    ) -> anyhow::Result<ArcBlock> {
        // // TODO: why does this happen?!?! seems to only happen with uncled blocks
        // // TODO: maybe we should do try_get_with?
        // // TODO: maybe we should just continue. this only seems to happen when an older block is received
        // warn!(
        //     "Missing connection_head_block in block_hashes. Fetching now. hash={}. other={}",
        //     connection_head_hash, conn_name
        // );

        // this option should almost always be populated. if the connection reconnects at a bad time it might not be available though
        let rpc = web3_connections.conns.get(rpc_name);

        web3_connections.block(authorization, hash, rpc).await
    }

    // TODO: do this during insert/remove?
    #[instrument(level = "trace")]
    pub(self) async fn highest_block(
        &self,
        authorization: &Arc<Authorization>,
        web3_connections: &Web3Connections,
    ) -> Option<ArcBlock> {
        let mut checked_heads = HashSet::with_capacity(self.rpc_name_to_hash.len());
        let mut highest_block = None::<ArcBlock>;

        for (rpc_name, rpc_head_hash) in self.rpc_name_to_hash.iter() {
            // don't waste time checking the same hash multiple times
            if checked_heads.contains(rpc_head_hash) {
                continue;
            }

            let rpc_block = match self
                .get_block_from_rpc(rpc_name, rpc_head_hash, authorization, web3_connections)
                .await
            {
                Ok(x) => x,
                Err(err) => {
                    warn!(
                        "failed getting block {} from {} while finding highest block number: {:?}",
                        rpc_head_hash, rpc_name, err,
                    );
                    continue;
                }
            };

            checked_heads.insert(rpc_head_hash);

            // if this is the first block we've tried
            // or if this rpc's newest block has a higher number
            // we used to check total difficulty, but that isn't a thing anymore on ETH
            // TODO: we still need total difficulty on some other PoW chains. whats annoying is it isn't considered part of the "block header" just the block. so websockets don't return it
            let highest_num = highest_block
                .as_ref()
                .map(|x| x.number.expect("blocks here should always have a number"));
            let rpc_num = rpc_block.as_ref().number;

            if rpc_num > highest_num {
                highest_block = Some(rpc_block);
            }
        }

        highest_block
    }

    #[instrument(level = "trace")]
    pub(self) async fn consensus_head_connections(
        &self,
        authorization: &Arc<Authorization>,
        web3_connections: &Web3Connections,
    ) -> anyhow::Result<ConsensusConnections> {
        let mut maybe_head_block = match self.highest_block(authorization, web3_connections).await {
            None => return Err(anyhow::anyhow!("No blocks known")),
            Some(x) => x,
        };

        let num_known = self.rpc_name_to_hash.len();

        // track rpcs on this heaviest chain so we can build a new ConsensusConnections
        let mut highest_rpcs = HashSet::<&str>::new();
        // a running total of the soft limits covered by the rpcs that agree on the head block
        let mut highest_rpcs_sum_soft_limit: u32 = 0;
        // TODO: also track highest_rpcs_sum_hard_limit? llama doesn't need this, so it can wait

        // check the highest work block for a set of rpcs that can serve our request load
        // if it doesn't have enough rpcs for our request load, check the parent block
        // TODO: loop for how many parent blocks? we don't want to serve blocks that are too far behind. probably different per chain
        // TODO: this loop is pretty long. any way to clean up this code?
        for _ in 0..6 {
            let maybe_head_hash = maybe_head_block
                .hash
                .as_ref()
                .expect("blocks here always need hashes");

            // find all rpcs with maybe_head_block as their current head
            for (rpc_name, rpc_head_hash) in self.rpc_name_to_hash.iter() {
                if rpc_head_hash != maybe_head_hash {
                    // connection is not on the desired block
                    continue;
                }
                if highest_rpcs.contains(rpc_name.as_str()) {
                    // connection is on a child block
                    continue;
                }

                if let Some(rpc) = web3_connections.conns.get(rpc_name.as_str()) {
                    highest_rpcs.insert(rpc_name);
                    highest_rpcs_sum_soft_limit += rpc.soft_limit;
                } else {
                    // i don't think this is an error. i think its just if a reconnect is currently happening
                    warn!("connection missing: {}", rpc_name);
                }
            }

            if highest_rpcs_sum_soft_limit >= web3_connections.min_sum_soft_limit
                && highest_rpcs.len() >= web3_connections.min_head_rpcs
            {
                // we have enough servers with enough requests
                break;
            }

            // not enough rpcs yet. check the parent block
            if let Some(parent_block) = web3_connections
                .block_hashes
                .get(&maybe_head_block.parent_hash)
            {
                // trace!(
                //     child=%maybe_head_hash, parent=%parent_block.hash.unwrap(), "avoiding thundering herd",
                // );

                maybe_head_block = parent_block;
                continue;
            } else {
                if num_known < web3_connections.min_head_rpcs {
                    return Err(anyhow::anyhow!(
                        "not enough rpcs connected: {}/{}/{}",
                        highest_rpcs.len(),
                        num_known,
                        web3_connections.min_head_rpcs,
                    ));
                } else {
                    let soft_limit_percent = (highest_rpcs_sum_soft_limit as f32
                        / web3_connections.min_sum_soft_limit as f32)
                        * 100.0;

                    return Err(anyhow::anyhow!(
                        "ran out of parents to check. rpcs {}/{}/{}. soft limit: {:.2}% ({}/{})",
                        highest_rpcs.len(),
                        num_known,
                        web3_connections.min_head_rpcs,
                        highest_rpcs_sum_soft_limit,
                        web3_connections.min_sum_soft_limit,
                        soft_limit_percent,
                    ));
                }
            }
        }

        // TODO: if consensus_head_rpcs.is_empty, try another method of finding the head block. will need to change the return Err above into breaks.

        // we've done all the searching for the heaviest block that we can
        if highest_rpcs.len() < web3_connections.min_head_rpcs
            || highest_rpcs_sum_soft_limit < web3_connections.min_sum_soft_limit
        {
            // if we get here, not enough servers are synced. return an error
            let soft_limit_percent = (highest_rpcs_sum_soft_limit as f32
                / web3_connections.min_sum_soft_limit as f32)
                * 100.0;

            return Err(anyhow::anyhow!(
                "Not enough resources. rpcs {}/{}/{}. soft limit: {:.2}% ({}/{})",
                highest_rpcs.len(),
                num_known,
                web3_connections.min_head_rpcs,
                highest_rpcs_sum_soft_limit,
                web3_connections.min_sum_soft_limit,
                soft_limit_percent,
            ));
        }

        // success! this block has enough soft limit and nodes on it (or on later blocks)
        let conns: Vec<Arc<Web3Connection>> = highest_rpcs
            .into_iter()
            .filter_map(|conn_name| web3_connections.conns.get(conn_name).cloned())
            .collect();

        // TODO: DEBUG only check
        let _ = maybe_head_block
            .hash
            .expect("head blocks always have hashes");
        let _ = maybe_head_block
            .number
            .expect("head blocks always have numbers");

        let consensus_head_block: SavedBlock = maybe_head_block.into();

        Ok(ConsensusConnections {
            head_block: Some(consensus_head_block),
            conns,
            num_checked_conns: self.rpc_name_to_hash.len(),
            includes_backups: self.includes_backups,
        })
    }
}

/// A ConsensusConnections builder that tracks all connection heads across multiple groups of servers
#[derive(Debug)]
pub struct ConsensusFinder {
    /// only main servers
    main: ConnectionsGroup,
    /// main and backup servers
    all: ConnectionsGroup,
}

impl Default for ConsensusFinder {
    #[instrument(level = "trace")]
    fn default() -> Self {
        Self {
            main: ConnectionsGroup::without_backups(),
            all: ConnectionsGroup::with_backups(),
        }
    }
}

impl ConsensusFinder {
    #[instrument(level = "trace")]
    fn remove(&mut self, rpc: &Web3Connection) -> Option<H256> {
        // TODO: should we have multiple backup tiers? (remote datacenters vs third party)
        if !rpc.backup {
            self.main.remove(rpc);
        }
        self.all.remove(rpc)
    }

    #[instrument(level = "trace")]
    fn insert(&mut self, rpc: &Web3Connection, new_hash: H256) -> Option<H256> {
        // TODO: should we have multiple backup tiers? (remote datacenters vs third party)
        if !rpc.backup {
            self.main.insert(rpc, new_hash);
        }
        self.all.insert(rpc, new_hash)
    }

    /// Update our tracking of the rpc and return true if something changed
    #[instrument(level = "trace")]
    async fn update_rpc(
        &mut self,
        rpc_head_block: Option<SavedBlock>,
        rpc: Arc<Web3Connection>,
        // we need this so we can save the block to caches. i don't like it though. maybe we should use a lazy_static Cache wrapper that has a "save_block" method?. i generally dislike globals but i also dislike all the types having to pass eachother around
        web3_connections: &Web3Connections,
    ) -> anyhow::Result<bool> {
        // add the rpc's block to connection_heads, or remove the rpc from connection_heads
        let changed = match rpc_head_block {
            Some(mut rpc_head_block) => {
                // we don't know if its on the heaviest chain yet
                rpc_head_block.block = web3_connections
                    .save_block(rpc_head_block.block, false)
                    .await?;

                // we used to remove here if the block was too far behind. but it just made things more complicated

                let rpc_head_hash = rpc_head_block.hash();

                if let Some(prev_hash) = self.insert(&rpc, rpc_head_hash) {
                    if prev_hash == rpc_head_hash {
                        // this block was already sent by this rpc. return early
                        false
                    } else {
                        // new block for this rpc
                        true
                    }
                } else {
                    // first block for this rpc
                    true
                }
            }
            None => {
                if self.remove(&rpc).is_none() {
                    // this rpc was already removed
                    false
                } else {
                    // rpc head changed from being synced to not
                    true
                }
            }
        };

        Ok(changed)
    }

    // TODO: this could definitely be cleaner. i don't like the error handling/unwrapping
    #[instrument(level = "trace")]
    async fn best_consensus_connections(
        &mut self,
        authorization: &Arc<Authorization>,
        web3_connections: &Web3Connections,
    ) -> ConsensusConnections {
        let highest_block_num = match self
            .all
            .highest_block(authorization, web3_connections)
            .await
        {
            None => {
                return ConsensusConnections::default();
            }
            Some(x) => x.number.expect("blocks here should always have a number"),
        };

        // TODO: also needs to be not less than our current head
        let mut min_block_num = highest_block_num.saturating_sub(U64::from(5));

        // we also want to be sure we don't ever go backwards!
        if let Some(current_consensus_head_num) = web3_connections.head_block_num() {
            min_block_num = min_block_num.max(current_consensus_head_num);
        }

        // TODO: pass `min_block_num` to consensus_head_connections?
        let consensus_head_for_main = self
            .main
            .consensus_head_connections(authorization, web3_connections)
            .await
            .map_err(|err| err.context("cannot use main group"));

        let consensus_num_for_main = consensus_head_for_main
            .as_ref()
            .ok()
            .map(|x| x.head_block.as_ref().unwrap().number());

        if let Some(consensus_num_for_main) = consensus_num_for_main {
            if consensus_num_for_main >= min_block_num {
                return consensus_head_for_main.unwrap();
            }
        }

        // TODO: pass `min_block_num` to consensus_head_connections?
        let consensus_connections_for_all = match self
            .all
            .consensus_head_connections(authorization, web3_connections)
            .await
        {
            Err(err) => {
                if self.all.rpc_name_to_hash.len() < web3_connections.min_head_rpcs {
                    debug!("No consensus head yet: {}", err);
                }
                return ConsensusConnections::default();
            }
            Ok(x) => x,
        };

        let consensus_num_for_all = consensus_connections_for_all
            .head_block
            .as_ref()
            .map(|x| x.number());

        if consensus_num_for_all > consensus_num_for_main {
            if consensus_num_for_all < Some(min_block_num) {
                // TODO: this should have an alarm in sentry
                error!("CONSENSUS HEAD w/ BACKUP NODES IS VERY OLD!");
            }
            consensus_connections_for_all
        } else {
            if let Ok(x) = consensus_head_for_main {
                error!("CONSENSUS HEAD IS VERY OLD! Backup RPCs did not improve this situation");
                x
            } else {
                // TODO: i don't think we need this error. and i doublt we'll ever even get here
                error!("NO CONSENSUS HEAD!");
                ConsensusConnections::default()
            }
        }
    }
}
