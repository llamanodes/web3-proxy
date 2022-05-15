///! Load balanced communication with a group of web3 providers
use derive_more::From;
use ethers::prelude::H256;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use governor::clock::{QuantaClock, QuantaInstant};
use governor::NotUntil;
use hashbrown::HashMap;
use serde_json::value::RawValue;
use std::cmp;
use std::fmt;
use std::sync::atomic::{self, AtomicU64};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, trace, warn};

use crate::config::Web3ConnectionConfig;
use crate::connection::{ActiveRequestHandle, Web3Connection};

#[derive(Clone, Default)]
struct SyncedConnections {
    head_block_number: u64,
    head_block_hash: H256,
    inner: Vec<Arc<Web3Connection>>,
}

impl fmt::Debug for SyncedConnections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("SyncedConnections").finish_non_exhaustive()
    }
}

impl SyncedConnections {
    fn new(max_connections: usize) -> Self {
        let inner = Vec::with_capacity(max_connections);

        Self {
            head_block_number: 0,
            head_block_hash: Default::default(),
            inner,
        }
    }
}

/// A collection of web3 connections. Sends requests either the current best server or all servers.
#[derive(From)]
pub struct Web3Connections {
    inner: Vec<Arc<Web3Connection>>,
    /// TODO: what is the best type for this? Heavy reads with writes every few seconds. When writes happen, there is a burst of them
    /// TODO: we probably need a better lock on this
    synced_connections: RwLock<SyncedConnections>,
    best_head_block_number: Arc<AtomicU64>,
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
    pub async fn try_new(
        chain_id: usize,
        best_head_block_number: Arc<AtomicU64>,
        servers: Vec<Web3ConnectionConfig>,
        http_client: Option<reqwest::Client>,
        clock: &QuantaClock,
        subscribe_heads: bool,
    ) -> anyhow::Result<Arc<Self>> {
        let mut connections = vec![];

        let num_connections = servers.len();

        for server_config in servers.into_iter() {
            match server_config
                .try_build(clock, chain_id, http_client.clone())
                .await
            {
                Ok(connection) => connections.push(connection),
                Err(e) => warn!("Unable to connect to a server! {:?}", e),
            }
        }

        // TODO: exit if no connections?

        let connections = Arc::new(Self {
            best_head_block_number: best_head_block_number.clone(),
            inner: connections,
            synced_connections: RwLock::new(SyncedConnections::new(num_connections)),
        });

        if subscribe_heads {
            let (block_sender, block_receiver) = flume::unbounded();

            {
                let connections = Arc::clone(&connections);
                tokio::spawn(async move { connections.update_synced_rpcs(block_receiver).await });
            }

            for connection in connections.inner.iter() {
                // subscribe to new heads in a spawned future
                // TODO: channel instead. then we can have one future with write access to a left-right?
                let connection = Arc::clone(connection);
                let block_sender = block_sender.clone();
                tokio::spawn(async move {
                    let url = connection.url().to_string();

                    // TODO: instead of passing Some(connections), pass Some(channel_sender). Then listen on the receiver below to keep local heads up-to-date
                    if let Err(e) = connection.subscribe_new_heads(block_sender).await {
                        warn!("new_heads error on {}: {:?}", url, e);
                    }
                });
            }
        }

        Ok(connections)
    }

    pub fn head_block_number(&self) -> u64 {
        self.best_head_block_number.load(atomic::Ordering::Acquire)
    }

    /// Send the same request to all the handles. Returning the fastest successful result.
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

            let handle = tokio::spawn(async move {
                let response: Box<RawValue> =
                    active_request_handle.request(&method, &params).await?;

                // send the first good response to a one shot channel. that way we respond quickly
                // drop the result because errors are expected after the first send
                response_sender.send(Ok(response)).map_err(Into::into)
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
        if response_sender.send(e).is_ok() {
            // if we were able to send an error, then we never sent a success
            return Err(anyhow::anyhow!("no successful responses"));
        } else {
            // if sending the error failed. the other side must be closed (which means we sent a success earlier)
            Ok(())
        }
    }

    /// TODO: possible dead lock here. investigate more. probably refactor
    // #[instrument]
    pub async fn update_synced_rpcs(
        &self,
        block_receiver: flume::Receiver<(u64, H256, Arc<Web3Connection>)>,
    ) -> anyhow::Result<()> {
        while let Ok((new_block_num, new_block_hash, rpc)) = block_receiver.recv_async().await {
            if new_block_num == 0 {
                warn!("{} is still syncing", rpc);
                continue;
            }

            // TODO: experiment with different locks and such here
            let mut synced_connections = self.synced_connections.write().await;

            // TODO: double check this logic
            match new_block_num.cmp(&synced_connections.head_block_number) {
                cmp::Ordering::Greater => {
                    // the rpc's newest block is the new overall best block
                    info!("new head block {} from {}", new_block_num, rpc);

                    synced_connections.inner.clear();
                    synced_connections.inner.push(rpc);

                    synced_connections.head_block_number = new_block_num;
                    synced_connections.head_block_hash = new_block_hash;
                }
                cmp::Ordering::Equal => {
                    if new_block_hash != synced_connections.head_block_hash {
                        // same height, but different chain
                        warn!(
                            "chain is forked! {} has {}. First #{} was {}",
                            rpc, new_block_hash, new_block_num, synced_connections.head_block_hash
                        );
                        continue;
                    }

                    // do not clear synced_connections.
                    // we just want to add this rpc to the end
                    synced_connections.inner.push(rpc);
                }
                cmp::Ordering::Less => {
                    // this isn't the best block in the tier. don't do anything
                    continue;
                }
            }

            // TODO: better log
            trace!("Now synced: {:?}", synced_connections.inner);
        }

        Ok(())
    }

    /// get the best available rpc server
    pub async fn next_upstream_server(
        &self,
    ) -> Result<ActiveRequestHandle, Option<NotUntil<QuantaInstant>>> {
        let mut earliest_not_until = None;

        // TODO: this clone is probably not the best way to do this
        let mut synced_rpc_indexes = self.synced_connections.read().await.inner.clone();

        // // TODO: how should we include the soft limit? floats are slower than integer math
        // let a = a as f32 / self.soft_limit as f32;
        // let b = b as f32 / other.soft_limit as f32;

        // TODO: better key!
        let sort_cache: HashMap<String, (f32, u32)> = synced_rpc_indexes
            .iter()
            .map(|connection| {
                // TODO: better key!
                let key = format!("{}", connection);
                let active_requests = connection.active_requests();
                let soft_limit = connection.soft_limit();

                let utilization = active_requests as f32 / soft_limit as f32;

                (key, (utilization, soft_limit))
            })
            .collect();

        // TODO: i think we might need to load active connections and then
        synced_rpc_indexes.sort_unstable_by(|a, b| {
            // TODO: better keys
            let a_key = format!("{}", a);
            let b_key = format!("{}", b);

            let (a_utilization, a_soft_limit) = sort_cache.get(&a_key).unwrap();
            let (b_utilization, b_soft_limit) = sort_cache.get(&b_key).unwrap();

            // TODO: i'm comparing floats. crap
            match a_utilization
                .partial_cmp(b_utilization)
                .unwrap_or(cmp::Ordering::Equal)
            {
                cmp::Ordering::Equal => a_soft_limit.cmp(b_soft_limit),
                x => x,
            }
        });

        for selected_rpc in synced_rpc_indexes.into_iter() {
            // increment our connection counter
            match selected_rpc.try_request_handle() {
                Err(not_until) => {
                    earliest_possible(&mut earliest_not_until, not_until);
                }
                Ok(handle) => {
                    trace!("next server on {:?}: {:?}", self, selected_rpc);
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
