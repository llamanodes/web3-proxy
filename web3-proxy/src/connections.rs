///! Load balanced communication with a group of web3 providers
use derive_more::From;
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
use tracing::{debug, info, trace, warn};

use crate::config::Web3ConnectionConfig;
use crate::connection::{ActiveRequestHandle, Web3Connection};

#[derive(Clone, Default)]
struct SyncedConnections {
    head_block_number: u64,
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
        let inner = Vec::with_capacity(max_connections);

        Self {
            head_block_number: 0,
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
            for connection in connections.inner.iter() {
                // subscribe to new heads in a spawned future
                // TODO: channel instead. then we can have one future with write access to a left-right?
                let connection = Arc::clone(connection);
                let connections = connections.clone();
                tokio::spawn(async move {
                    let url = connection.url().to_string();

                    // TODO: instead of passing Some(connections), pass Some(channel_sender). Then listen on the receiver below to keep local heads up-to-date
                    if let Err(e) = connection.new_heads(Some(connections)).await {
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

    pub async fn update_synced_rpcs(&self, rpc: &Arc<Web3Connection>) -> anyhow::Result<()> {
        let mut synced_connections = self.synced_connections.write().await;

        let new_block = rpc.head_block_number();

        if new_block == 0 {
            warn!("{} is still syncing", rpc);
            return Ok(());
        }

        let current_best_block_number = synced_connections.head_block_number;

        let overall_best_head_block = self.head_block_number();

        // TODO: double check this logic
        match (
            new_block.cmp(&overall_best_head_block),
            new_block.cmp(&current_best_block_number),
        ) {
            (cmp::Ordering::Greater, cmp::Ordering::Greater) => {
                // the rpc's newest block is the new overall best block
                synced_connections.inner.clear();

                synced_connections.head_block_number = new_block;

                // TODO: what ordering?
                match self.best_head_block_number.compare_exchange(
                    overall_best_head_block,
                    new_block,
                    atomic::Ordering::AcqRel,
                    atomic::Ordering::Acquire,
                ) {
                    Ok(_) => {
                        info!("new head block {} from {}", new_block, rpc);
                    }
                    Err(current_best_block_number) => {
                        // actually, there was a race and this ended up not being the latest block. return now without adding this rpc to the synced list
                        debug!(
                            "behind {} on {:?}: {}",
                            current_best_block_number, rpc, new_block
                        );
                        return Ok(());
                    }
                }
            }
            (cmp::Ordering::Equal, cmp::Ordering::Less) => {
                // no need to do anything
                return Ok(());
            }
            (cmp::Ordering::Greater, cmp::Ordering::Less) => {
                // this isn't the best block in the tier. don't do anything
                return Ok(());
            }
            (cmp::Ordering::Equal, cmp::Ordering::Equal) => {
                // this rpc tier is synced, and it isn't the first to this block
            }
            (cmp::Ordering::Less, cmp::Ordering::Less) => {
                // this rpc is behind the best and the tier. don't do anything
                return Ok(());
            }
            (cmp::Ordering::Less, cmp::Ordering::Equal) => {
                // this rpc is behind the best. but is an improvement for the tier
                return Ok(());
            }
            (cmp::Ordering::Less, cmp::Ordering::Greater) => {
                // this rpc is behind the best, but it is catching up
                synced_connections.inner.clear();

                synced_connections.head_block_number = new_block;

                // return now because this isn't actually synced
                return Ok(());
            }
            (cmp::Ordering::Equal, cmp::Ordering::Greater) => {
                // we caught up to another tier
                synced_connections.inner.clear();

                synced_connections.head_block_number = new_block;
            }
            (cmp::Ordering::Greater, cmp::Ordering::Equal) => {
                // TODO: what should we do? i think we got here because we aren't using atomics properly
                // the overall block hasn't yet updated, but our internal block has
                // TODO: maybe we should
            }
        }

        let rpc_index = self
            .inner
            .iter()
            .position(|x| x.url() == rpc.url())
            .unwrap();

        // TODO: hopefully nothing ends up in here twice. Greater+Equal might do that to us
        synced_connections.inner.push(rpc_index);

        trace!("Now synced {:?}: {:?}", self, synced_connections.inner);

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

        let sort_cache: HashMap<usize, (f32, u32)> = synced_rpc_indexes
            .iter()
            .map(|synced_index| {
                let key = *synced_index;

                let connection = self.inner.get(*synced_index).unwrap();

                let active_requests = connection.active_requests();
                let soft_limit = connection.soft_limit();

                let utilization = active_requests as f32 / soft_limit as f32;

                (key, (utilization, soft_limit))
            })
            .collect();

        // TODO: i think we might need to load active connections and then
        synced_rpc_indexes.sort_unstable_by(|a, b| {
            let (a_utilization, a_soft_limit) = sort_cache.get(a).unwrap();
            let (b_utilization, b_soft_limit) = sort_cache.get(b).unwrap();

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
            let selected_rpc = self.inner.get(selected_rpc).unwrap();

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
    /// even fetches if they aren't in sync. This is useful for broadcasting signed transactions
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
