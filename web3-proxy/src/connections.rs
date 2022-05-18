///! Load balanced communication with a group of web3 providers
use arc_swap::ArcSwap;
use derive_more::From;
use ethers::prelude::H256;
use futures::future::join_all;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use governor::clock::{QuantaClock, QuantaInstant};
use governor::NotUntil;
use serde_json::value::RawValue;
use std::cmp;
use std::fmt;
use std::sync::Arc;
use tokio::task;
use tracing::Instrument;
use tracing::{info, info_span, instrument, trace, warn};

use crate::config::Web3ConnectionConfig;
use crate::connection::{ActiveRequestHandle, Web3Connection};

#[derive(Clone)]
struct SyncedConnections {
    head_block_num: u64,
    head_block_hash: H256,
    inner: Vec<usize>,
}

impl fmt::Debug for SyncedConnections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("SyncedConnections").finish_non_exhaustive()
    }
}

impl SyncedConnections {
    fn new(max_connections: usize) -> Self {
        Self {
            head_block_num: 0,
            head_block_hash: Default::default(),
            inner: Vec::with_capacity(max_connections),
        }
    }

    pub fn get_head_block_hash(&self) -> &H256 {
        &self.head_block_hash
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
        chain_id: usize,
        servers: Vec<Web3ConnectionConfig>,
        http_client: Option<reqwest::Client>,
        clock: &QuantaClock,
    ) -> anyhow::Result<Arc<Self>> {
        let num_connections = servers.len();

        // turn configs into connections
        let mut connections = Vec::with_capacity(num_connections);
        for server_config in servers.into_iter() {
            match server_config
                .try_build(clock, chain_id, http_client.clone())
                .await
            {
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

    pub async fn subscribe_heads(self: &Arc<Self>) {
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
                    // TODO: proper spann
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

        join_all(handles).await;
    }

    pub fn get_head_block_hash(&self) -> H256 {
        *self.synced_connections.load().get_head_block_hash()
    }

    /// Send the same request to all the handles. Returning the fastest successful result.
    #[instrument(skip_all)]
    pub async fn try_send_parallel_requests(
        self: Arc<Self>,
        active_request_handles: Vec<ActiveRequestHandle>,
        method: String,
        params: Option<Box<RawValue>>,
        response_sender: flume::Sender<anyhow::Result<Box<RawValue>>>,
    ) -> anyhow::Result<()> {
        // TODO: if only 1 active_request_handles, do self.try_send_request
        let mut unordered_futures = FuturesUnordered::new();

        for active_request_handle in active_request_handles {
            // clone things so we can pass them to a future
            let method = method.clone();
            let params = params.clone();
            let response_sender = response_sender.clone();

            let handle = task::Builder::default()
                .name("send_request")
                .spawn(async move {
                    let response: Box<RawValue> =
                        active_request_handle.request(&method, &params).await?;

                    // send the first good response to a one shot channel. that way we respond quickly
                    // drop the result because errors are expected after the first send
                    response_sender
                        .send_async(Ok(response))
                        .await
                        .map_err(Into::into)
                });

            unordered_futures.push(handle);
        }

        // TODO: use iterators instead of pushing into a vec?
        let mut errs = vec![];
        if let Some(x) = unordered_futures.next().await {
            match x.unwrap() {
                Ok(_) => {}
                Err(e) => {
                    // TODO: better errors
                    warn!("Got an error sending request: {}", e);
                    errs.push(e);
                }
            }
        }

        // get the first error (if any)
        // TODO: why collect multiple errors if we only pop one?
        let e = if !errs.is_empty() {
            Err(errs.pop().unwrap())
        } else {
            Err(anyhow::anyhow!("no successful responses"))
        };

        // send the error to the channel
        if response_sender.send_async(e).await.is_ok() {
            // if we were able to send an error, then we never sent a success
            return Err(anyhow::anyhow!("no successful responses"));
        } else {
            // if sending the error failed. the other side must be closed (which means we sent a success earlier)
            Ok(())
        }
    }

    /// TODO: possible dead lock here. investigate more. probably refactor
    /// TODO: move parts of this onto SyncedConnections?
    #[instrument(skip_all)]
    async fn update_synced_rpcs(
        &self,
        block_receiver: flume::Receiver<(u64, H256, usize)>,
    ) -> anyhow::Result<()> {
        let max_connections = self.inner.len();

        let mut connection_states: Vec<(u64, H256)> = Vec::with_capacity(max_connections);
        let mut head_block_hash = H256::zero();
        let mut head_block_num = 0u64;

        let mut synced_connections = SyncedConnections::new(max_connections);

        while let Ok((new_block_num, new_block_hash, rpc_id)) = block_receiver.recv_async().await {
            if new_block_num == 0 {
                // TODO: show the actual rpc url?
                warn!("rpc #{} is still syncing", rpc_id);
            }

            // TODO: span with rpc in it, too
            let span = info_span!("new_block", new_block_num);
            let _enter = span.enter();

            connection_states.insert(rpc_id, (new_block_num, new_block_hash));

            // TODO: do something to update the synced blocks
            match new_block_num.cmp(&head_block_num) {
                cmp::Ordering::Greater => {
                    // the rpc's newest block is the new overall best block
                    info!("new head from #{}", rpc_id);

                    synced_connections.inner.clear();
                    synced_connections.inner.push(rpc_id);

                    head_block_num = new_block_num;
                    head_block_hash = new_block_hash;
                }
                cmp::Ordering::Equal => {
                    if new_block_hash != head_block_hash {
                        // same height, but different chain
                        // TODO: anything else we should do? set some "nextSafeBlockHeight" to delay sending transactions?
                        // TODO: sometimes a node changes its block. if that happens, a new block is probably right behind this one
                        warn!(
                            "chain is forked at #{}! {} has {}. {} rpcs have {}",
                            new_block_num,
                            rpc_id,
                            new_block_hash,
                            synced_connections.inner.len(),
                            head_block_hash
                        );
                        // TODO: don't continue. check to see which head block is more popular!
                        continue;
                    }

                    // do not clear synced_connections.
                    // we just want to add this rpc to the end
                    synced_connections.inner.push(rpc_id);
                }
                cmp::Ordering::Less => {
                    // this isn't the best block in the tier. don't do anything
                    continue;
                }
            }

            // the synced connections have changed
            let new_data = Arc::new(synced_connections.clone());

            // TODO: only do this if there are 2 nodes synced to this block?
            // do the arcswap
            self.synced_connections.swap(new_data);
        }

        // TODO: if there was an error, we should return it
        warn!("block_receiver exited!");

        Ok(())
    }

    /// get the best available rpc server
    #[instrument(skip_all)]
    pub async fn next_upstream_server(
        &self,
    ) -> Result<ActiveRequestHandle, Option<NotUntil<QuantaInstant>>> {
        let mut earliest_not_until = None;

        let mut synced_rpc_indexes = self.synced_connections.load().inner.clone();

        let sort_cache: Vec<(f32, u32)> = synced_rpc_indexes
            .iter()
            .map(|rpc_id| {
                let rpc = self.inner.get(*rpc_id).unwrap();

                let active_requests = rpc.active_requests();
                let soft_limit = rpc.soft_limit();

                // TODO: how should we include the soft limit? floats are slower than integer math
                let utilization = active_requests as f32 / soft_limit as f32;

                (utilization, soft_limit)
            })
            .collect();

        // TODO: i think we might need to load active connections and then
        synced_rpc_indexes.sort_unstable_by(|a, b| {
            let (a_utilization, a_soft_limit) = sort_cache.get(*a).unwrap();
            let (b_utilization, b_soft_limit) = sort_cache.get(*b).unwrap();

            // TODO: i'm comparing floats. crap
            match a_utilization
                .partial_cmp(b_utilization)
                .unwrap_or(cmp::Ordering::Equal)
            {
                cmp::Ordering::Equal => a_soft_limit.cmp(b_soft_limit),
                x => x,
            }
        });

        for rpc_id in synced_rpc_indexes.into_iter() {
            let rpc = self.inner.get(rpc_id).unwrap();

            // increment our connection counter
            match rpc.try_request_handle() {
                Err(not_until) => {
                    earliest_possible(&mut earliest_not_until, not_until);
                }
                Ok(handle) => {
                    trace!("next server on {:?}: {:?}", self, rpc_id);
                    return Ok(handle);
                }
            }
        }

        // TODO: this is too verbose
        // warn!("no servers on {:?}! {:?}", self, earliest_not_until);

        // this might be None
        Err(earliest_not_until)
    }

    /// get all rpc servers that are not rate limited
    /// returns servers even if they aren't in sync. This is useful for broadcasting signed transactions
    pub fn get_upstream_servers(
        &self,
    ) -> Result<Vec<ActiveRequestHandle>, Option<NotUntil<QuantaInstant>>> {
        let mut earliest_not_until = None;
        // TODO: with capacity?
        let mut selected_rpcs = vec![];

        for connection in self.inner.iter() {
            // check rate limits and increment our connection counter
            match connection.try_request_handle() {
                Err(not_until) => {
                    earliest_possible(&mut earliest_not_until, not_until);
                    // this rpc is not available. skip it
                }
                Ok(handle) => selected_rpcs.push(handle),
            }
        }

        if !selected_rpcs.is_empty() {
            return Ok(selected_rpcs);
        }

        // return the earliest not_until (if no rpcs are synced, this will be None)
        Err(earliest_not_until)
    }
}

fn earliest_possible(
    earliest_not_until_option: &mut Option<NotUntil<QuantaInstant>>,
    new_not_until: NotUntil<QuantaInstant>,
) {
    match earliest_not_until_option.as_ref() {
        None => *earliest_not_until_option = Some(new_not_until),
        Some(earliest_not_until) => {
            let earliest_possible = earliest_not_until.earliest_possible();
            let new_earliest_possible = new_not_until.earliest_possible();

            if earliest_possible > new_earliest_possible {
                *earliest_not_until_option = Some(new_not_until);
            }
        }
    }
}
