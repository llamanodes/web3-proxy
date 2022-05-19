///! Load balanced communication with a group of web3 providers
use arc_swap::ArcSwap;
use derive_more::From;
use ethers::prelude::H256;
use futures::future::join_all;
use hashbrown::{HashMap, HashSet};
use std::cmp;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use tokio::task;
use tracing::Instrument;
use tracing::{info, info_span, instrument, warn};

use crate::connection::Web3Connection;

#[derive(Clone, Debug)]
struct SyncedConnections {
    head_block_num: u64,
    head_block_hash: H256,
    inner: HashSet<usize>,
}

impl SyncedConnections {
    fn new(max_connections: usize) -> Self {
        Self {
            head_block_num: 0,
            head_block_hash: Default::default(),
            inner: HashSet::with_capacity(max_connections),
        }
    }
}

/// A collection of web3 connections. Sends requests either the current best server or all servers.
#[derive(From)]
pub struct Web3Connections {
    inner: Vec<Arc<Web3Connection>>,
    synced_connections: ArcSwap<SyncedConnections>,
}

impl fmt::Debug for Web3Connections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("Web3Connections")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl Web3Connections {
    // #[instrument(name = "try_new_Web3Connections", skip_all)]
    pub async fn try_new(
        chain_id: u64,
        servers: Vec<String>,
        http_client: Option<reqwest::Client>,
    ) -> anyhow::Result<Arc<Self>> {
        let num_connections = servers.len();

        // turn configs into connections
        let mut connections = Vec::with_capacity(num_connections);
        for rpc_url in servers.into_iter() {
            match Web3Connection::try_new(chain_id, rpc_url, http_client.clone()).await {
                Ok(connection) => connections.push(connection),
                Err(e) => warn!("Unable to connect to a server! {:?}", e),
            }
        }

        if connections.len() < 2 {
            // TODO: less than 3? what should we do here?
            return Err(anyhow::anyhow!(
                "need at least 2 connections when subscribing to heads!"
            ));
        }

        let synced_connections = SyncedConnections::new(num_connections);

        let connections = Arc::new(Self {
            inner: connections,
            synced_connections: ArcSwap::new(Arc::new(synced_connections)),
        });

        Ok(connections)
    }

    pub async fn subscribe_heads(self: &Arc<Self>) -> anyhow::Result<()> {
        let (block_sender, block_receiver) = flume::unbounded();

        let mut handles = vec![];

        for (rpc_id, connection) in self.inner.iter().enumerate() {
            // subscribe to new heads in a spawned future
            // TODO: channel instead. then we can have one future with write access to a left-right?
            let connection = Arc::clone(connection);
            let block_sender = block_sender.clone();

            // let url = connection.url().to_string();

            let handle = task::Builder::default()
                .name("subscribe_new_heads")
                .spawn(async move {
                    // loop to automatically reconnect
                    // TODO: make this cancellable?
                    // TODO: instead of passing Some(connections), pass Some(channel_sender). Then listen on the receiver below to keep local heads up-to-date
                    // TODO: proper span
                    connection.check_chain_id().await?;

                    connection
                        .subscribe_new_heads(rpc_id, block_sender.clone(), true)
                        .instrument(tracing::info_span!("url"))
                        .await
                });

            handles.push(handle);
        }

        let connections = Arc::clone(self);
        let handle = task::Builder::default()
            .name("update_synced_rpcs")
            .spawn(async move { connections.update_synced_rpcs(block_receiver).await });

        handles.push(handle);

        for x in join_all(handles).await {
            match x {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return Err(e),
                Err(e) => return Err(e.into()),
            }
        }

        Ok(())
    }

    /// TODO: move parts of this onto SyncedConnections?
    #[instrument(skip_all)]
    async fn update_synced_rpcs(
        &self,
        block_receiver: flume::Receiver<(u64, H256, usize)>,
    ) -> anyhow::Result<()> {
        let max_connections = self.inner.len();

        let mut connection_states: HashMap<usize, (u64, H256)> =
            HashMap::with_capacity(max_connections);

        let mut pending_synced_connections = SyncedConnections::new(max_connections);

        while let Ok((new_block_num, new_block_hash, rpc_id)) = block_receiver.recv_async().await {
            if new_block_num == 0 {
                // TODO: show the actual rpc url?
                warn!("rpc #{} is still syncing", rpc_id);
            }

            // TODO: span with rpc in it, too
            // TODO: make sure i'm doing this span right
            let span = info_span!("new_block", new_block_num);
            let _enter = span.enter();

            connection_states.insert(rpc_id, (new_block_num, new_block_hash));

            // TODO: do something to update the synced blocks
            match new_block_num.cmp(&pending_synced_connections.head_block_num) {
                cmp::Ordering::Greater => {
                    // the rpc's newest block is the new overall best block
                    info!(rpc_id, "new head");

                    pending_synced_connections.inner.clear();
                    pending_synced_connections.inner.insert(rpc_id);

                    pending_synced_connections.head_block_num = new_block_num;

                    // TODO: if the parent hash isn't our previous best block, ignore it
                    pending_synced_connections.head_block_hash = new_block_hash;
                }
                cmp::Ordering::Equal => {
                    if new_block_hash == pending_synced_connections.head_block_hash {
                        // this rpc has caught up with the best known head
                        // do not clear synced_connections.
                        // we just want to add this rpc to the end
                        // TODO: HashSet here? i think we get dupes if we don't
                        pending_synced_connections.inner.insert(rpc_id);
                    } else {
                        // same height, but different chain

                        // check connection_states to see which head block is more popular!
                        let mut rpc_ids_by_block: BTreeMap<H256, Vec<usize>> = BTreeMap::new();

                        let mut synced_rpcs = 0;

                        // TODO: what order should we iterate in? track last update time, too?
                        for (rpc_id, (block_num, block_hash)) in connection_states.iter() {
                            if *block_num != new_block_num {
                                // this connection isn't synced. we don't care what hash it has
                                continue;
                            }

                            synced_rpcs += 1;

                            let count = rpc_ids_by_block
                                .entry(*block_hash)
                                .or_insert_with(|| Vec::with_capacity(max_connections - 1));

                            count.push(*rpc_id);
                        }

                        let most_common_head_hash = rpc_ids_by_block
                            .iter()
                            .max_by(|a, b| a.1.len().cmp(&b.1.len()))
                            .map(|(k, _v)| k)
                            .unwrap();

                        warn!(
                            "chain is forked! {} possible heads. {}/{}/{} rpcs have {}",
                            rpc_ids_by_block.len(),
                            rpc_ids_by_block.get(most_common_head_hash).unwrap().len(),
                            synced_rpcs,
                            max_connections,
                            most_common_head_hash
                        );

                        // this isn't the best block in the tier. make sure this rpc isn't included
                        if new_block_hash != *most_common_head_hash {
                            pending_synced_connections.inner.remove(&rpc_id);
                        }

                        // TODO: if pending_synced_connections hasn't changed. continue
                    }
                }
                cmp::Ordering::Less => {
                    // this isn't the best block in the tier. don't do anything
                    if !pending_synced_connections.inner.remove(&rpc_id) {
                        // we didn't remove anything. nothing more to do
                        continue;
                    }
                    // we removed. don't continue so that we update self.synced_connections
                }
            }

            // the synced connections have changed
            let synced_connections = Arc::new(pending_synced_connections.clone());

            info!("new synced_connections: {:?}", synced_connections);

            // TODO: only do this if there are 2 nodes synced to this block?
            // do the arcswap
            self.synced_connections.swap(synced_connections);
        }

        // TODO: if there was an error, we should return it
        Err(anyhow::anyhow!("block_receiver exited!"))
    }
}
