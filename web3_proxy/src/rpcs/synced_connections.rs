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
    pub(super) head_block_num: U64,
    pub(super) head_block_hash: H256,
    // TODO: this should be able to serialize, but it isn't
    #[serde(skip_serializing)]
    pub(super) conns: IndexSet<Arc<Web3Connection>>,
}

impl fmt::Debug for SyncedConnections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        // TODO: print the actual conns?
        f.debug_struct("SyncedConnections")
            .field("head_num", &self.head_block_num)
            .field("head_hash", &self.head_block_hash)
            .field("num_conns", &self.conns.len())
            .finish_non_exhaustive()
    }
}

impl Web3Connections {
    pub fn head_block(&self) -> (U64, H256) {
        let synced_connections = self.synced_connections.load();

        (
            synced_connections.head_block_num,
            synced_connections.head_block_hash,
        )
    }

    pub fn head_block_hash(&self) -> H256 {
        self.synced_connections.load().head_block_hash
    }

    pub fn head_block_num(&self) -> U64 {
        self.synced_connections.load().head_block_num
    }

    pub fn synced(&self) -> bool {
        !self.synced_connections.load().conns.is_empty()
    }

    pub fn num_synced_rpcs(&self) -> usize {
        self.synced_connections.load().conns.len()
    }
}
