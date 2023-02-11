use super::blockchain::{ArcBlock, SavedBlock};
use super::many::Web3Rpcs;
use super::one::Web3Rpc;
use ethers::prelude::{H256, U64};
use serde::Serialize;
use std::fmt;
use std::sync::Arc;

/// A collection of Web3Rpcs that are on the same block.
/// Serialize is so we can print it on our debug endpoint
#[derive(Clone, Default, Serialize)]
pub struct ConsensusWeb3Rpcs {
    // TODO: store ArcBlock instead?
    pub(super) head_block: Option<SavedBlock>,
    // TODO: this should be able to serialize, but it isn't
    #[serde(skip_serializing)]
    pub(super) conns: Vec<Arc<Web3Rpc>>,
    pub(super) num_checked_conns: usize,
    pub(super) includes_backups: bool,
}

impl ConsensusWeb3Rpcs {
    pub fn num_conns(&self) -> usize {
        self.conns.len()
    }

    pub fn sum_soft_limit(&self) -> u32 {
        self.conns.iter().fold(0, |sum, rpc| sum + rpc.soft_limit)
    }

    // TODO: sum_hard_limit?
}

impl fmt::Debug for ConsensusWeb3Rpcs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        // TODO: print the actual conns?
        f.debug_struct("ConsensusConnections")
            .field("head_block", &self.head_block)
            .field("num_conns", &self.conns.len())
            .finish_non_exhaustive()
    }
}

impl Web3Rpcs {
    pub fn head_block(&self) -> Option<ArcBlock> {
        self.watch_consensus_head_receiver
            .as_ref()
            .map(|x| x.borrow().clone())
    }

    pub fn head_block_hash(&self) -> Option<H256> {
        self.head_block().and_then(|x| x.hash)
    }

    pub fn head_block_num(&self) -> Option<U64> {
        self.head_block().and_then(|x| x.number)
    }

    pub fn synced(&self) -> bool {
        !self
            .watch_consensus_connections_sender
            .borrow()
            .conns
            .is_empty()
    }

    pub fn num_synced_rpcs(&self) -> usize {
        self.watch_consensus_connections_sender.borrow().conns.len()
    }
}
