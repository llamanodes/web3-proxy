//! Keep track of the blockchain as seen by a Web3Rpcs.
use super::consensus::ConsensusFinder;
use super::many::Web3Rpcs;
use super::one::Web3Rpc;
use super::transactions::TxStatus;
use crate::config::{average_block_interval, BlockAndRpc};
use crate::errors::{Web3ProxyError, Web3ProxyErrorContext, Web3ProxyResult};
use crate::frontend::authorization::Authorization;
use derive_more::From;
use ethers::prelude::{Block, TxHash, H256, U64};
use moka::future::Cache;
use serde::ser::SerializeStruct;
use serde::Serialize;
use serde_json::json;
use std::hash::Hash;
use std::time::Duration;
use std::{fmt::Display, sync::Arc};
use tokio::sync::{broadcast, mpsc};
use tokio::time::timeout;
use tracing::{debug, error, warn};

// TODO: type for Hydrated Blocks with their full transactions?
pub type ArcBlock = Arc<Block<TxHash>>;

pub type BlocksByHashCache = Cache<H256, Web3ProxyBlock>;
pub type BlocksByNumberCache = Cache<U64, H256>;

/// A block and its age.
#[derive(Clone, Debug, Default, From)]
pub struct Web3ProxyBlock {
    pub block: ArcBlock,
    /// number of seconds this block was behind the current time when received
    /// this is only set if the block is from a subscription
    pub received_age: Option<u64>,
}

impl Serialize for Web3ProxyBlock {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // TODO: i'm not sure about this name
        let mut state = serializer.serialize_struct("saved_block", 2)?;

        state.serialize_field("age", &self.age())?;

        let block = json!({
            "hash": self.block.hash,
            "parent_hash": self.block.parent_hash,
            "number": self.block.number,
            "timestamp": self.block.timestamp,
        });

        state.serialize_field("block", &block)?;

        state.end()
    }
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

impl Eq for Web3ProxyBlock {}

impl Hash for Web3ProxyBlock {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.block.hash.hash(state);
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
        x.received_age = Some(x.age().as_secs());

        Some(x)
    }

    pub fn age(&self) -> Duration {
        let now = chrono::Utc::now().timestamp();

        let block_timestamp = self.block.timestamp.as_u32() as i64;

        let x = if block_timestamp < now {
            // this server is still syncing from too far away to serve requests
            // u64 is safe because we checked equality above
            (now - block_timestamp) as u64
        } else {
            0
        };

        Duration::from_secs(x)
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

    pub fn uncles(&self) -> &[H256] {
        &self.block.uncles
    }
}

impl TryFrom<ArcBlock> for Web3ProxyBlock {
    type Error = Web3ProxyError;

    fn try_from(x: ArcBlock) -> Result<Self, Self::Error> {
        if x.number.is_none() || x.hash.is_none() {
            return Err(Web3ProxyError::NoBlockNumberOrHash);
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
            self.age().as_secs()
        )
    }
}

impl Web3Rpcs {
    /// add a block to our mappings and track the heaviest chain
    pub async fn try_cache_block(
        &self,
        block: Web3ProxyBlock,
        consensus_head: bool,
    ) -> Web3ProxyResult<Web3ProxyBlock> {
        let block_hash = *block.hash();

        // TODO: i think we can rearrange this function to make it faster on the hot path
        if block_hash.is_zero() {
            debug!("Skipping block without hash!");
            return Ok(block);
        }

        // this block is very likely already in block_hashes

        if consensus_head {
            let block_num = block.number();

            // TODO: if there is an existing entry with a different block_hash,
            // TODO: use entry api to handle changing existing entries
            self.blocks_by_number.insert(*block_num, block_hash).await;

            for uncle in block.uncles() {
                self.blocks_by_hash.invalidate(uncle).await;
                // TODO: save uncles somewhere?
            }

            // loop to make sure parent hashes match our caches
            // set the first ancestor to the blocks' parent hash. but keep going up the chain
            if let Some(parent_num) = block.number().checked_sub(1.into()) {
                struct Ancestor {
                    num: U64,
                    hash: H256,
                }
                let mut ancestor = Ancestor {
                    num: parent_num,
                    hash: *block.parent_hash(),
                };
                loop {
                    let ancestor_number_to_hash_entry = self
                        .blocks_by_number
                        .entry_by_ref(&ancestor.num)
                        .or_insert(ancestor.hash)
                        .await;

                    if *ancestor_number_to_hash_entry.value() == ancestor.hash {
                        // the existing number entry matches. all good
                        break;
                    }

                    // oh no! ancestor_number_to_hash_entry is different

                    // remove the uncled entry in blocks_by_hash
                    // we will look it up later if necessary
                    self.blocks_by_hash
                        .invalidate(ancestor_number_to_hash_entry.value())
                        .await;

                    // TODO: delete any cached entries for eth_getBlockByHash or eth_getBlockByNumber

                    // TODO: race on this drop and insert?
                    drop(ancestor_number_to_hash_entry);

                    // update the entry in blocks_by_number
                    self.blocks_by_number
                        .insert(ancestor.num, ancestor.hash)
                        .await;

                    // try to check the parent of this ancestor
                    if let Some(ancestor_block) = self.blocks_by_hash.get(&ancestor.hash) {
                        match ancestor_block.number().checked_sub(1.into()) {
                            None => break,
                            Some(ancestor_parent_num) => {
                                ancestor = Ancestor {
                                    num: ancestor_parent_num,
                                    hash: *ancestor_block.parent_hash(),
                                }
                            }
                        }
                    } else {
                        break;
                    }
                }
            }
        }

        let block = self
            .blocks_by_hash
            .get_with_by_ref(&block_hash, async move { block })
            .await;

        Ok(block)
    }

    /// Get a block from caches with fallback.
    /// Will query a specific node or the best available.
    pub async fn block(
        &self,
        authorization: &Arc<Authorization>,
        hash: &H256,
        rpc: Option<&Arc<Web3Rpc>>,
        max_tries: Option<usize>,
        max_wait: Option<Duration>,
    ) -> Web3ProxyResult<Web3ProxyBlock> {
        // first, try to get the hash from our cache
        // the cache is set last, so if its here, its everywhere
        // TODO: use try_get_with
        if let Some(block) = self.blocks_by_hash.get(hash) {
            // double check that it matches the blocks_by_number cache
            let cached_hash = self
                .blocks_by_number
                .get_with_by_ref(block.number(), async { *hash })
                .await;

            if cached_hash == *hash {
                return Ok(block);
            }

            // hashes don't match! this block must be in the middle of being uncled
            // TODO: check known uncles
        }

        if hash == &H256::zero() {
            // TODO: think more about this
            return Err(Web3ProxyError::UnknownBlockHash(*hash));
        }

        // block not in cache. we need to ask an rpc for it
        let get_block_params = (*hash, false);

        let mut block: Option<ArcBlock> = if let Some(rpc) = rpc {
            // ask a specific rpc
            // TODO: request_with_metadata would probably be better than authorized_request
            rpc.authorized_request::<_, Option<ArcBlock>>(
                "eth_getBlockByHash",
                &get_block_params,
                authorization,
                None,
                max_tries,
                max_wait,
            )
            .await?
        } else {
            None
        };

        if block.is_none() {
            // try by asking any rpc
            // TODO: retry if "Requested data is not available"
            // TODO: request_with_metadata instead of internal_request
            block = self
                .internal_request::<_, Option<ArcBlock>>(
                    "eth_getBlockByHash",
                    &get_block_params,
                    max_tries,
                    max_wait,
                )
                .await?;
        };

        match block {
            Some(block) => {
                let block = self.try_cache_block(block.try_into()?, false).await?;
                Ok(block)
            }
            None => Err(Web3ProxyError::UnknownBlockHash(*hash)),
        }
    }

    /// Convenience method to get the cannonical block at a given block height.
    pub async fn block_hash(
        &self,
        authorization: &Arc<Authorization>,
        num: &U64,
    ) -> Web3ProxyResult<(H256, u64)> {
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
    ) -> Web3ProxyResult<(Web3ProxyBlock, u64)> {
        // we only have blocks by hash now
        // maybe save them during save_block in a blocks_by_number Cache<U64, Vec<ArcBlock>>
        // if theres multiple, use petgraph to find the one on the main chain (and remove the others if they have enough confirmations)

        let mut consensus_head_receiver = self
            .watch_head_block
            .as_ref()
            .web3_context("need new head subscriptions to fetch cannonical_block")?
            .subscribe();

        // be sure the requested block num exists
        // TODO: is this okay? what if we aren't synced?!
        let mut head_block_num = *consensus_head_receiver
            .borrow_and_update()
            .as_ref()
            .web3_context("no consensus head block")?
            .number();

        loop {
            if num <= &head_block_num {
                break;
            }

            debug!(%head_block_num, %num, "waiting for future block");

            consensus_head_receiver.changed().await?;

            if let Some(head) = consensus_head_receiver.borrow_and_update().as_ref() {
                head_block_num = *head.number();
            }
        }

        let block_depth = (head_block_num - num).as_u64();

        // try to get the hash from our cache
        // deref to not keep the lock open
        if let Some(block_hash) = self.blocks_by_number.get(num) {
            // TODO: sometimes this needs to fetch the block. why? i thought block_numbers would only be set if the block hash was set
            // TODO: configurable max wait and rpc
            let block = self
                .block(authorization, &block_hash, None, Some(3), None)
                .await?;

            return Ok((block, block_depth));
        }

        // block number not in cache. we need to ask an rpc for it
        // TODO: this error is too broad
        let response = self
            .internal_request::<_, Option<ArcBlock>>(
                "eth_getBlockByNumber",
                &(*num, false),
                Some(3),
                None,
            )
            .await?
            .ok_or(Web3ProxyError::NoBlocksKnown)?;

        let block = Web3ProxyBlock::try_from(response)?;

        // the block was fetched using eth_getBlockByNumber, so it should have all fields and be on the heaviest chain
        let block = self.try_cache_block(block, true).await?;

        Ok((block, block_depth))
    }

    pub(super) async fn process_incoming_blocks(
        &self,
        authorization: &Arc<Authorization>,
        mut block_receiver: mpsc::UnboundedReceiver<BlockAndRpc>,
        // TODO: document that this is a watch sender and not a broadcast! if things get busy, blocks might get missed
        // Geth's subscriptions have the same potential for skipping blocks.
        pending_tx_sender: Option<broadcast::Sender<TxStatus>>,
    ) -> Web3ProxyResult<()> {
        let mut consensus_finder =
            ConsensusFinder::new(Some(self.max_head_block_age), Some(self.max_head_block_lag));

        // TODO: what timeout on block receiver? we want to keep consensus_finder fresh so that server tiers are correct
        let double_block_time = average_block_interval(self.chain_id).mul_f32(2.0);

        let mut had_first_success = false;

        loop {
            match timeout(double_block_time, block_receiver.recv()).await {
                Ok(Some((new_block, rpc))) => {
                    let rpc_name = rpc.name.clone();
                    let rpc_is_backup = rpc.backup;

                    // TODO: what timeout on this?
                    match timeout(
                        Duration::from_secs(1),
                        consensus_finder.process_block_from_rpc(
                            self,
                            authorization,
                            new_block,
                            rpc,
                            &pending_tx_sender,
                        ),
                    )
                    .await
                    {
                        Ok(Ok(_)) => had_first_success = true,
                        Ok(Err(err)) => {
                            if had_first_success {
                                error!(
                                    "error while processing block from rpc {}: {:#?}",
                                    rpc_name, err
                                );
                            } else {
                                debug!(
                                    "startup error while processing block from rpc {}: {:#?}",
                                    rpc_name, err
                                );
                            }
                        }
                        Err(timeout) => {
                            if rpc_is_backup {
                                debug!(
                                    ?timeout,
                                    "timeout while processing block from {}", rpc_name
                                );
                            } else {
                                warn!(?timeout, "timeout while processing block from {}", rpc_name);
                            }
                        }
                    }
                }
                Ok(None) => {
                    // TODO: panic is probably too much, but getting here is definitely not good
                    return Err(anyhow::anyhow!("block_receiver on {} exited", self).into());
                }
                Err(_) => {
                    // TODO: what timeout on this?
                    match timeout(
                        Duration::from_secs(2),
                        consensus_finder.refresh(self, authorization, None, None),
                    )
                    .await
                    {
                        Ok(Ok(_)) => {}
                        Ok(Err(err)) => {
                            error!("error while refreshing consensus finder: {:#?}", err);
                        }
                        Err(timeout) => {
                            error!("timeout while refreshing consensus finder: {:#?}", timeout);
                        }
                    }
                }
            }
        }
    }
}
