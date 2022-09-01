use super::blockchain::BlockId;
use super::connection::Web3Connection;
use super::connections::Web3Connections;
use ethers::prelude::{H256, U64};
use indexmap::IndexSet;
use serde::Serialize;
use std::fmt;
use std::sync::Arc;

/// A collection of Web3Connections that are on the same block.
/// Serialize is so we can print it on our debug endpoint
#[derive(Clone, Default, Serialize)]
pub struct SyncedConnections {
    // TODO: store ArcBlock instead?
    pub(super) head_block_id: Option<BlockId>,
    // TODO: this should be able to serialize, but it isn't
    #[serde(skip_serializing)]
    pub(super) conns: IndexSet<Arc<Web3Connection>>,
}

impl fmt::Debug for SyncedConnections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        // TODO: print the actual conns?
        f.debug_struct("SyncedConnections")
            .field("head_block_id", &self.head_block_id)
            .field("num_conns", &self.conns.len())
            .finish_non_exhaustive()
    }
}

impl Web3Connections {
    pub fn head_block_id(&self) -> Option<BlockId> {
        self.synced_connections.load().head_block_id.clone()
    }

    pub fn head_block_hash(&self) -> Option<H256> {
        self.synced_connections
            .load()
            .head_block_id
            .as_ref()
            .map(|head_block_id| head_block_id.hash)
    }

    pub fn head_block_num(&self) -> Option<U64> {
        self.synced_connections
            .load()
            .head_block_id
            .as_ref()
            .map(|head_block_id| head_block_id.num)
    }

    pub fn synced(&self) -> bool {
        !self.synced_connections.load().conns.is_empty()
    }

    pub fn num_synced_rpcs(&self) -> usize {
        self.synced_connections.load().conns.len()
    }
}
