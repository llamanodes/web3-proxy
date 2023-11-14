//! Keep track of the blockchain as seen by a Web3Rpcs.
use super::consensus::ConsensusFinder;
use super::many::Web3Rpcs;
use crate::config::{average_block_interval, BlockAndRpc};
use crate::errors::{Web3ProxyError, Web3ProxyResult};
use ethers::prelude::{Block, TxHash, H256, U64};
use moka::future::Cache;
use serde::ser::SerializeStruct;
use serde::Serialize;
use serde_json::json;
use std::hash::Hash;
use std::time::Duration;
use std::{fmt::Display, sync::Arc};
use tokio::select;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::{debug, error, warn};

// TODO: type for Hydrated Blocks with their full transactions?
pub type ArcBlock = Arc<Block<TxHash>>;

pub type BlocksByHashCache = Cache<H256, Web3ProxyBlock>;
pub type BlocksByNumberCache = Cache<U64, H256>;

/// A block and its age with a less verbose serialized format
/// This does **not** implement Default. We rarely want a block with number 0 and hash 0.
#[derive(Clone, Debug)]
pub struct Web3ProxyBlock(pub ArcBlock);

impl Serialize for Web3ProxyBlock {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // TODO: i'm not sure about this name
        let mut state = serializer.serialize_struct("saved_block", 2)?;

        state.serialize_field("age", &self.age().as_secs_f32())?;

        let block = json!({
            "hash": self.0.hash,
            "parent_hash": self.0.parent_hash,
            "number": self.0.number,
            "timestamp": self.0.timestamp,
        });

        state.serialize_field("block", &block)?;

        state.end()
    }
}

impl PartialEq for Web3ProxyBlock {
    fn eq(&self, other: &Self) -> bool {
        match (self.0.hash, other.0.hash) {
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
        self.0.hash.hash(state);
    }
}

impl Web3ProxyBlock {
    /// A new block has arrived over a subscription. skip it if its empty
    pub fn try_new(block: ArcBlock) -> Option<Self> {
        if block.number.is_none() || block.hash.is_none() {
            return None;
        }

        Some(Self(block))
    }

    pub fn age(&self) -> Duration {
        let now = chrono::Utc::now().timestamp();

        let block_timestamp = self.0.timestamp.as_u32() as i64;

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
        &self.0.parent_hash
    }

    #[inline(always)]
    pub fn hash(&self) -> &H256 {
        self.0.hash.as_ref().expect("saved blocks must have a hash")
    }

    #[inline(always)]
    pub fn number(&self) -> U64 {
        self.0.number.expect("saved blocks must have a number")
    }

    #[inline(always)]
    pub fn transactions(&self) -> &[TxHash] {
        &self.0.transactions
    }

    #[inline(always)]
    pub fn uncles(&self) -> &[H256] {
        &self.0.uncles
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

impl TryFrom<ArcBlock> for Web3ProxyBlock {
    type Error = Web3ProxyError;

    fn try_from(block: ArcBlock) -> Result<Self, Self::Error> {
        Self::try_new(block).ok_or(Web3ProxyError::NoBlocksKnown)
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
            self.blocks_by_number.insert(block_num, block_hash).await;

            for uncle in block.uncles() {
                self.blocks_by_hash.invalidate(uncle).await;
                // TODO: save uncles somewhere?
            }

            // loop to make sure parent hashes match our caches
            // set the first ancestor to the blocks' parent hash. but keep going up the chain
            if let Some(parent_num) = block.number().checked_sub(1.into()) {
                self.blocks_by_number
                    .insert(parent_num, *block.parent_hash())
                    .await;
            }
        }

        let block = self
            .blocks_by_hash
            .get_with_by_ref(&block_hash, async move { block })
            .await;

        Ok(block)
    }

    pub(super) async fn process_incoming_blocks(
        &self,
        mut block_and_rpc_receiver: mpsc::UnboundedReceiver<BlockAndRpc>,
    ) -> Web3ProxyResult<()> {
        if self.watch_head_block.is_none() {
            return Ok(());
        }

        // TODO: should this be spawned and then we just hold onto the handle here?
        let mut consensus_finder =
            ConsensusFinder::new(Some(self.max_head_block_age), self.max_head_block_lag);

        // TODO: what timeout on block receiver? we want to keep consensus_finder fresh so that server tiers are correct
        let triple_block_time = average_block_interval(self.chain_id).mul_f32(3.0);

        loop {
            select! {
                x = block_and_rpc_receiver.recv() => {
                    match x {
                        Some((new_block, rpc)) => {
                            let rpc_name = rpc.name.clone();

                            // TODO: we used to have a timeout on this, but i think it was obscuring a bug
                            match consensus_finder
                                .process_block_from_rpc(self, new_block, rpc)
                                .await
                            {
                                Ok(_) => {},
                                Err(err) => {
                                    error!(
                                        "error while processing block from rpc {}: {:#?}",
                                        rpc_name, err
                                    );
                                }
                            }
                        }
                        None => {
                            // TODO: panic is probably too much, but getting here is definitely not good
                            return Err(anyhow::anyhow!("block_receiver on {} exited", self).into());
                        }
                    }
                }
                _ = sleep(triple_block_time) => {
                    // TODO: what timeout on this?
                    match consensus_finder.refresh(self, None, None).await {
                        Ok(_) => {
                            warn!("had to refresh consensus finder. is the network going slow?");
                        }
                        Err(err) => {
                            error!("error while refreshing consensus finder: {:#?}", err);
                        }
                    }
                }
            }
        }
    }
}
