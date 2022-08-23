use super::Web3Connection;
use ethers::prelude::{H256, U64};
use indexmap::IndexSet;
use serde::Serialize;
use std::sync::Arc;

/// A collection of Web3Connections that are on the same block.
/// Serialize is so we can print it on our debug endpoint
#[derive(Clone, Default, Serialize)]
pub struct SyncedConnections {
    pub(super) head_block_num: U64,
    pub(super) head_block_hash: H256,
    // TODO: this should be able to serialize, but it isn't
    #[serde(skip_serializing)]
    pub(super) conns: IndexSet<Arc<Web3Connection>>,
}
