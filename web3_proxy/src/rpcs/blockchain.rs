use super::consensus::ConsensusFinder;
use super::many::Web3Rpcs;
///! Keep track of the blockchain as seen by a Web3Rpcs.
use super::one::Web3Rpc;
use super::transactions::TxStatus;
use crate::frontend::authorization::Authorization;
use crate::{config::BlockAndRpc, jsonrpc::JsonRpcRequest};
use anyhow::{anyhow, Context};
use derive_more::From;
use ethers::prelude::{Block, TxHash, H256, U64};
use hashbrown::HashSet;
use log::{debug, error, trace, warn, Level};
use moka::future::Cache;
use serde::Serialize;
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{cmp::Ordering, fmt::Display, sync::Arc};
use tokio::sync::{broadcast, watch};
use tokio::time::Duration;

// TODO: type for Hydrated Blocks with their full transactions?
pub type ArcBlock = Arc<Block<TxHash>>;

pub type BlockHashesCache = Cache<H256, Web3ProxyBlock, hashbrown::hash_map::DefaultHashBuilder>;

/// A block and its age.
#[derive(Clone, Debug, Default, From, Serialize)]
pub struct Web3ProxyBlock {
    pub block: ArcBlock,
    /// number of seconds this block was behind the current time when received
    /// this is only set if the block is from a subscription
    pub received_age: Option<u64>,
}

impl PartialEq for Web3ProxyBlock {
    fn eq(&self, other: &Self) -> bool {
        match (self.block.hash, other.block.hash) {
            (None, None) => true,
            (Some(_), None) => false,
            (None, Some(_)) => false,
            (Some(s), Some(o)) => s == o,
        }
    }
}

impl Web3ProxyBlock {
    /// A new block has arrived over a subscription
    pub fn try_new(block: ArcBlock) -> Option<Self> {
        if block.number.is_none() || block.hash.is_none() {
            return None;
        }

        let mut x = Self {
            block,
            received_age: None,
        };

        // no need to recalulate lag every time
        // if the head block gets too old, a health check restarts this connection
        // TODO: emit a stat for received_age
        x.received_age = Some(x.age());

        Some(x)
    }

    pub fn age(&self) -> u64 {
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

    #[inline(always)]
    pub fn parent_hash(&self) -> &H256 {
        &self.block.parent_hash
    }

    #[inline(always)]
    pub fn hash(&self) -> &H256 {
        self.block
            .hash
            .as_ref()
            .expect("saved blocks must have a hash")
    }

    #[inline(always)]
    pub fn number(&self) -> &U64 {
        self.block
            .number
            .as_ref()
            .expect("saved blocks must have a number")
    }
}

impl TryFrom<ArcBlock> for Web3ProxyBlock {
    type Error = anyhow::Error;

    fn try_from(x: ArcBlock) -> Result<Self, Self::Error> {
        if x.number.is_none() || x.hash.is_none() {
            return Err(anyhow!("Blocks here must have a number of hash"));
        }

        let b = Web3ProxyBlock {
            block: x,
            received_age: None,
        };

        Ok(b)
    }
}

impl Display for Web3ProxyBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({}, {}s old)",
            self.number(),
            self.hash(),
            self.age()
        )
    }
}

impl Web3Rpcs {
    /// add a block to our mappings and track the heaviest chain
    pub async fn try_cache_block(
        &self,
        block: Web3ProxyBlock,
        heaviest_chain: bool,
    ) -> anyhow::Result<Web3ProxyBlock> {
        // TODO: i think we can rearrange this function to make it faster on the hot path
        let block_hash = block.hash();

        // skip Block::default()
        if block_hash.is_zero() {
            debug!("Skipping block without hash!");
            return Ok(block);
        }

        let block_num = block.number();

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
    pub async fn block(
        &self,
        authorization: &Arc<Authorization>,
        hash: &H256,
        rpc: Option<&Arc<Web3Rpc>>,
    ) -> anyhow::Result<Web3ProxyBlock> {
        // first, try to get the hash from our cache
        // the cache is set last, so if its here, its everywhere
        // TODO: use try_get_with
        if let Some(block) = self.block_hashes.get(hash) {
            return Ok(block);
        }

        // block not in cache. we need to ask an rpc for it
        let get_block_params = (*hash, false);
        // TODO: if error, retry?
        let block: Web3ProxyBlock = match rpc {
            Some(rpc) => rpc
                .wait_for_request_handle(authorization, Some(Duration::from_secs(30)), None)
                .await?
                .request::<_, Option<ArcBlock>>(
                    "eth_getBlockByHash",
                    &json!(get_block_params),
                    Level::Error.into(),
                    None,
                )
                .await?
                .and_then(|x| {
                    if x.number.is_none() {
                        None
                    } else {
                        x.try_into().ok()
                    }
                })
                .context("no block!")?,
            None => {
                // TODO: helper for method+params => JsonRpcRequest
                // TODO: does this id matter?
                let request = json!({ "id": "1", "method": "eth_getBlockByHash", "params": get_block_params });
                let request: JsonRpcRequest = serde_json::from_value(request)?;

                // TODO: request_metadata? maybe we should put it in the authorization?
                // TODO: think more about this wait_for_sync
                let response = self
                    .try_send_best_consensus_head_connection(
                        authorization,
                        request,
                        None,
                        None,
                        None,
                    )
                    .await?;

                let block = response.result.context("failed fetching block")?;

                let block: Option<ArcBlock> = serde_json::from_str(block.get())?;

                let block: ArcBlock = block.context("no block in the response")?;

                // TODO: received time is going to be weird
                Web3ProxyBlock::try_from(block)?
            }
        };

        // the block was fetched using eth_getBlockByHash, so it should have all fields
        // TODO: fill in heaviest_chain! if the block is old enough, is this definitely true?
        let block = self.try_cache_block(block, false).await?;

        Ok(block)
    }

    /// Convenience method to get the cannonical block at a given block height.
    pub async fn block_hash(
        &self,
        authorization: &Arc<Authorization>,
        num: &U64,
    ) -> anyhow::Result<(H256, u64)> {
        let (block, block_depth) = self.cannonical_block(authorization, num).await?;

        let hash = *block.hash();

        Ok((hash, block_depth))
    }

    /// Get the heaviest chain's block from cache or backend rpc
    /// Caution! If a future block is requested, this might wait forever. Be sure to have a timeout outside of this!
    pub async fn cannonical_block(
        &self,
        authorization: &Arc<Authorization>,
        num: &U64,
    ) -> anyhow::Result<(Web3ProxyBlock, u64)> {
        // we only have blocks by hash now
        // maybe save them during save_block in a blocks_by_number Cache<U64, Vec<ArcBlock>>
        // if theres multiple, use petgraph to find the one on the main chain (and remove the others if they have enough confirmations)

        let mut consensus_head_receiver = self
            .watch_consensus_head_receiver
            .as_ref()
            .context("need new head subscriptions to fetch cannonical_block")?
            .clone();

        // be sure the requested block num exists
        // TODO: is this okay? what if we aren't synced?!
        let mut head_block_num = *consensus_head_receiver
            .borrow_and_update()
            .as_ref()
            .context("no consensus head block")?
            .number();

        loop {
            if num <= &head_block_num {
                break;
            }

            trace!("waiting for future block {} > {}", num, head_block_num);
            consensus_head_receiver.changed().await?;

            if let Some(head) = consensus_head_receiver.borrow_and_update().as_ref() {
                head_block_num = *head.number();
            }
        }

        let block_depth = (head_block_num - num).as_u64();

        // try to get the hash from our cache
        // deref to not keep the lock open
        if let Some(block_hash) = self.block_numbers.get(num) {
            // TODO: sometimes this needs to fetch the block. why? i thought block_numbers would only be set if the block hash was set
            // TODO: pass authorization through here?
            let block = self.block(authorization, &block_hash, None).await?;

            return Ok((block, block_depth));
        }

        // block number not in cache. we need to ask an rpc for it
        // TODO: helper for method+params => JsonRpcRequest
        let request = json!({ "jsonrpc": "2.0", "id": "1", "method": "eth_getBlockByNumber", "params": (num, false) });
        let request: JsonRpcRequest = serde_json::from_value(request)?;

        // TODO: if error, retry?
        // TODO: request_metadata or authorization?
        // we don't actually set min_block_needed here because all nodes have all blocks
        let response = self
            .try_send_best_consensus_head_connection(authorization, request, None, None, None)
            .await?;

        if let Some(err) = response.error {
            debug!("could not find canonical block {}: {:?}", num, err);
        }

        let raw_block = response.result.context("no cannonical block result")?;

        let block: ArcBlock = serde_json::from_str(raw_block.get())?;

        let block = Web3ProxyBlock::try_from(block)?;

        // the block was fetched using eth_getBlockByNumber, so it should have all fields and be on the heaviest chain
        let block = self.try_cache_block(block, true).await?;

        Ok((block, block_depth))
    }

    pub(super) async fn process_incoming_blocks(
        &self,
        authorization: &Arc<Authorization>,
        block_receiver: flume::Receiver<BlockAndRpc>,
        // TODO: document that this is a watch sender and not a broadcast! if things get busy, blocks might get missed
        // Geth's subscriptions have the same potential for skipping blocks.
        head_block_sender: watch::Sender<Option<Web3ProxyBlock>>,
        pending_tx_sender: Option<broadcast::Sender<TxStatus>>,
    ) -> anyhow::Result<()> {
        // TODO: indexmap or hashmap? what hasher? with_capacity?
        // TODO: this will grow unbounded. prune old heads on this at the same time we prune the graph?
        let configured_tiers: Vec<u64> = self
            .by_name
            .values()
            .map(|x| x.tier)
            .collect::<HashSet<u64>>()
            .into_iter()
            .collect();

        let mut connection_heads =
            ConsensusFinder::new(&configured_tiers, self.max_block_age, self.max_block_lag);

        loop {
            match block_receiver.recv_async().await {
                Ok((new_block, rpc)) => {
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
                        warn!("unable to process block from rpc {}: {:#?}", rpc_name, err);
                    }
                }
                Err(err) => {
                    warn!("block_receiver exited! {:#?}", err);
                    return Err(err.into());
                }
            }
        }
    }

    /// `connection_heads` is a mapping of rpc_names to head block hashes.
    /// self.blockchain_map is a mapping of hashes to the complete ArcBlock.
    /// TODO: return something?
    pub(crate) async fn process_block_from_rpc(
        &self,
        authorization: &Arc<Authorization>,
        consensus_finder: &mut ConsensusFinder,
        rpc_head_block: Option<Web3ProxyBlock>,
        rpc: Arc<Web3Rpc>,
        head_block_sender: &watch::Sender<Option<Web3ProxyBlock>>,
        pending_tx_sender: &Option<broadcast::Sender<TxStatus>>,
    ) -> anyhow::Result<()> {
        // TODO: how should we handle an error here?
        if !consensus_finder
            .update_rpc(rpc_head_block.clone(), rpc.clone(), self)
            .await
            .context("failed to update rpc")?
        {
            // nothing changed. no need to scan for a new consensus head
            return Ok(());
        }

        let new_synced_connections = consensus_finder
            .best_consensus_connections(authorization, self)
            .await
            .context("no consensus head block!")
            .map_err(|err| {
                self.watch_consensus_rpcs_sender
                    .send_replace(Arc::new(Default::default()));

                err
            })?;

        // TODO: what should we do if the block number of new_synced_connections is < old_synced_connections? wait?

        let backups_needed = new_synced_connections.backups_needed;
        let consensus_head_block = new_synced_connections.head_block.clone();
        let num_consensus_rpcs = new_synced_connections.num_conns();
        let num_active_rpcs = consensus_finder
            .all_rpcs_group()
            .map(|x| x.len())
            .unwrap_or_default();
        let total_rpcs = self.by_name.len();

        let old_consensus_head_connections = self
            .watch_consensus_rpcs_sender
            .send_replace(Arc::new(new_synced_connections));

        let backups_voted_str = if backups_needed { "B " } else { "" };

        if let Some(consensus_head_block) = consensus_head_block {
            match &old_consensus_head_connections.head_block {
                None => {
                    debug!(
                        "first {}{}/{}/{} block={}, rpc={}",
                        backups_voted_str,
                        num_consensus_rpcs,
                        num_active_rpcs,
                        total_rpcs,
                        consensus_head_block,
                        rpc,
                    );

                    if backups_needed {
                        // TODO: what else should be in this error?
                        warn!("Backup RPCs are in use!");
                    }

                    // this should already be cached
                    let consensus_head_block =
                        self.try_cache_block(consensus_head_block, true).await?;

                    head_block_sender
                        .send(Some(consensus_head_block))
                        .context("head_block_sender sending consensus_head_block")?;
                }
                Some(old_head_block) => {
                    // TODO: do this log item better
                    let rpc_head_str = rpc_head_block
                        .map(|x| x.to_string())
                        .unwrap_or_else(|| "None".to_string());

                    match consensus_head_block.number().cmp(&old_head_block.number()) {
                        Ordering::Equal => {
                            // multiple blocks with the same fork!
                            if consensus_head_block.hash() == old_head_block.hash() {
                                // no change in hash. no need to use head_block_sender
                                // TODO: trace level if rpc is backup
                                debug!(
                                    "con {}{}/{}/{} con={} rpc={}@{}",
                                    backups_voted_str,
                                    num_consensus_rpcs,
                                    num_active_rpcs,
                                    total_rpcs,
                                    consensus_head_block,
                                    rpc,
                                    rpc_head_str,
                                )
                            } else {
                                // hash changed
                                if backups_needed {
                                    // TODO: what else should be in this error?
                                    warn!("Backup RPCs are in use!");
                                }

                                debug!(
                                    "unc {}{}/{}/{} con_head={} old={} rpc={}@{}",
                                    backups_voted_str,
                                    num_consensus_rpcs,
                                    num_active_rpcs,
                                    total_rpcs,
                                    consensus_head_block,
                                    old_head_block,
                                    rpc,
                                    rpc_head_str,
                                );

                                let consensus_head_block = self
                                    .try_cache_block(consensus_head_block, true)
                                    .await
                                    .context("save consensus_head_block as heaviest chain")?;

                                head_block_sender
                                    .send(Some(consensus_head_block))
                                    .context("head_block_sender sending consensus_head_block")?;
                            }
                        }
                        Ordering::Less => {
                            // this is unlikely but possible
                            // TODO: better log
                            warn!(
                                "chain rolled back {}{}/{}/{} con={} old={} rpc={}@{}",
                                backups_voted_str,
                                num_consensus_rpcs,
                                num_active_rpcs,
                                total_rpcs,
                                consensus_head_block,
                                old_head_block,
                                rpc,
                                rpc_head_str,
                            );

                            if backups_needed {
                                // TODO: what else should be in this error?
                                warn!("Backup RPCs are in use!");
                            }

                            // TODO: tell save_block to remove any higher block numbers from the cache. not needed because we have other checks on requested blocks being > head, but still seems like a good idea
                            let consensus_head_block = self
                                .try_cache_block(consensus_head_block, true)
                                .await
                                .context(
                                    "save_block sending consensus_head_block as heaviest chain",
                                )?;

                            head_block_sender
                                .send(Some(consensus_head_block))
                                .context("head_block_sender sending consensus_head_block")?;
                        }
                        Ordering::Greater => {
                            debug!(
                                "new {}{}/{}/{} con={} rpc={}@{}",
                                backups_voted_str,
                                num_consensus_rpcs,
                                num_active_rpcs,
                                total_rpcs,
                                consensus_head_block,
                                rpc,
                                rpc_head_str,
                            );

                            if backups_needed {
                                // TODO: what else should be in this error?
                                warn!("Backup RPCs are in use!");
                            }

                            let consensus_head_block =
                                self.try_cache_block(consensus_head_block, true).await?;

                            head_block_sender.send(Some(consensus_head_block))?;
                        }
                    }
                }
            }
        } else {
            // TODO: do this log item better
            let rpc_head_str = rpc_head_block
                .map(|x| x.to_string())
                .unwrap_or_else(|| "None".to_string());

            if num_active_rpcs >= self.min_head_rpcs {
                // no consensus!!!
                error!(
                    "non {}{}/{}/{} rpc={}@{}",
                    backups_voted_str,
                    num_consensus_rpcs,
                    num_active_rpcs,
                    total_rpcs,
                    rpc,
                    rpc_head_str,
                );
            } else {
                // no consensus, but we do not have enough rpcs connected yet to panic
                debug!(
                    "non {}{}/{}/{} rpc={}@{}",
                    backups_voted_str,
                    num_consensus_rpcs,
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
