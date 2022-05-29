///! Load balanced communication with a group of web3 providers
use arc_swap::ArcSwap;
use counter::Counter;
use derive_more::From;
use ethers::prelude::{ProviderError, H256};
use futures::future::join_all;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use hashbrown::HashMap;
use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;
use serde_json::value::RawValue;
use std::cmp;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::task;
use tokio::time::sleep;
use tracing::Instrument;
use tracing::{debug, info, info_span, instrument, trace, warn};

use crate::config::Web3ConnectionConfig;
use crate::connection::{ActiveRequestHandle, Web3Connection};
use crate::jsonrpc::{JsonRpcForwardedResponse, JsonRpcRequest};

// Serialize so we can print it on our debug endpoint
#[derive(Clone, Default, Serialize)]
struct SyncedConnections {
    head_block_num: u64,
    head_block_hash: H256,
    inner: BTreeSet<usize>,
}

impl fmt::Debug for SyncedConnections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("SyncedConnections").finish_non_exhaustive()
    }
}

impl SyncedConnections {
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

impl Serialize for Web3Connections {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let inner: Vec<&Web3Connection> = self.inner.iter().map(|x| x.as_ref()).collect();

        // 3 is the number of fields in the struct.
        let mut state = serializer.serialize_struct("Web3Connections", 2)?;
        state.serialize_field("rpcs", &inner)?;
        state.serialize_field("synced_connections", &**self.synced_connections.load())?;
        state.end()
    }
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
        http_client: Option<&reqwest::Client>,
        rate_limiter: Option<&redis_cell_client::MultiplexedConnection>,
    ) -> anyhow::Result<Arc<Self>> {
        let num_connections = servers.len();

        // turn configs into connections
        let mut connections = Vec::with_capacity(num_connections);
        for server_config in servers.into_iter() {
            match server_config
                .try_build(rate_limiter, chain_id, http_client)
                .await
            {
                Ok(connection) => connections.push(connection),
                Err(e) => warn!("Unable to connect to a server! {:?}", e),
            }
        }

        // TODO: less than 3? what should we do here?
        if connections.len() < 2 {
            warn!("Only {} connection(s)!", connections.len());
        }

        let synced_connections = SyncedConnections::default();

        let connections = Arc::new(Self {
            inner: connections,
            synced_connections: ArcSwap::new(Arc::new(synced_connections)),
        });

        Ok(connections)
    }

    pub async fn subscribe_heads(self: &Arc<Self>) {
        // TODO: i don't think this needs to be very big
        let (block_sender, block_receiver) = flume::bounded(16);

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

        // TODO: do something with join_all's result
        join_all(handles).await;
    }

    pub fn get_head_block_hash(&self) -> H256 {
        *self.synced_connections.load().get_head_block_hash()
    }

    /// Send the same request to all the handles. Returning the most common success or most common error.
    #[instrument(skip_all)]
    pub async fn try_send_parallel_requests(
        &self,
        active_request_handles: Vec<ActiveRequestHandle>,
        method: &str,
        // TODO: remove this box once i figure out how to do the options
        params: Option<&RawValue>,
    ) -> Result<Box<RawValue>, ProviderError> {
        // TODO: if only 1 active_request_handles, do self.try_send_request

        let responses = active_request_handles
            .into_iter()
            .map(|active_request_handle| async move {
                let result: Result<Box<RawValue>, _> =
                    active_request_handle.request(method, params).await;
                result
            })
            .collect::<FuturesUnordered<_>>()
            .collect::<Vec<Result<Box<RawValue>, ProviderError>>>()
            .await;

        // TODO: Strings are not great keys, but we can't use RawValue or ProviderError as keys because they don't implement Hash or Eq
        let mut count_map: HashMap<String, Result<Box<RawValue>, ProviderError>> = HashMap::new();
        let mut counts: Counter<String> = Counter::new();
        let mut any_ok = false;
        for response in responses {
            let s = format!("{:?}", response);

            if count_map.get(&s).is_none() {
                if response.is_ok() {
                    any_ok = true;
                }

                count_map.insert(s.clone(), response);
            }

            counts.update([s].into_iter());
        }

        for (most_common, _) in counts.most_common_ordered() {
            let most_common = count_map.remove(&most_common).unwrap();

            if any_ok && most_common.is_err() {
                //  errors were more common, but we are going to skip them because we got an okay
                continue;
            } else {
                // return the most common
                return most_common;
            }
        }

        // TODO: what should we do if we get here? i don't think we will
        panic!("i don't think this is possible")
    }

    /// TODO: possible dead lock here. investigate more. probably refactor
    /// TODO: move parts of this onto SyncedConnections?
    // we don't instrument here because we put a span inside the while loop
    async fn update_synced_rpcs(
        &self,
        block_receiver: flume::Receiver<(u64, H256, usize)>,
    ) -> anyhow::Result<()> {
        let max_connections = self.inner.len();

        let mut connection_states: HashMap<usize, (u64, H256)> =
            HashMap::with_capacity(max_connections);

        let mut pending_synced_connections = SyncedConnections::default();

        while let Ok((new_block_num, new_block_hash, rpc_id)) = block_receiver.recv_async().await {
            // TODO: span with more in it?
            // TODO: make sure i'm doing this span right
            // TODO: show the actual rpc url?
            let span = info_span!("block_receiver", rpc_id, new_block_num);
            let _enter = span.enter();

            if new_block_num == 0 {
                warn!("rpc is still syncing");
            }

            connection_states.insert(rpc_id, (new_block_num, new_block_hash));

            // TODO: do something to update the synced blocks
            match new_block_num.cmp(&pending_synced_connections.head_block_num) {
                cmp::Ordering::Greater => {
                    // the rpc's newest block is the new overall best block
                    // TODO: if trace, do the full block hash?
                    // TODO: only accept this block if it is a child of the current head_block
                    info!("new head: {}", new_block_hash);

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

                        // this isn't the best block in the tier. don't do anything
                        if !pending_synced_connections.inner.remove(&rpc_id) {
                            // we didn't remove anything. nothing more to do
                            continue;
                        }
                        // we removed. don't continue so that we update self.synced_connections
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

            if synced_connections.inner.len() == max_connections {
                // TODO: more metrics
                debug!("all head: {}", new_block_hash);
            }

            trace!(
                "rpcs at {}: {:?}",
                synced_connections.head_block_hash,
                synced_connections.inner
            );

            // TODO: only publish if there are x (default 2) nodes synced to this block?
            // do the arcswap
            self.synced_connections.swap(synced_connections);
        }

        // TODO: if there was an error, we should return it
        warn!("block_receiver exited!");

        Ok(())
    }

    /// get the best available rpc server
    #[instrument(skip_all)]
    pub async fn next_upstream_server(&self) -> Result<ActiveRequestHandle, Option<Duration>> {
        let mut earliest_retry_after = None;

        let mut synced_rpc_ids: Vec<usize> = self
            .synced_connections
            .load()
            .inner
            .iter()
            .cloned()
            .collect();

        let sort_cache: HashMap<usize, (f32, u32)> = synced_rpc_ids
            .iter()
            .map(|rpc_id| {
                let rpc = self.inner.get(*rpc_id).unwrap();

                let active_requests = rpc.active_requests();
                let soft_limit = rpc.soft_limit();

                let utilization = active_requests as f32 / soft_limit as f32;

                (*rpc_id, (utilization, soft_limit))
            })
            .collect();

        synced_rpc_ids.sort_unstable_by(|a, b| {
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

        // now that the rpcs are sorted, try to get an active request handle for one of them
        for rpc_id in synced_rpc_ids.into_iter() {
            let rpc = self.inner.get(rpc_id).unwrap();

            // increment our connection counter
            match rpc.try_request_handle().await {
                Err(retry_after) => {
                    earliest_retry_after = earliest_retry_after.min(Some(retry_after));
                }
                Ok(handle) => {
                    trace!("next server on {:?}: {:?}", self, rpc_id);
                    return Ok(handle);
                }
            }
        }

        warn!("no servers on {:?}! {:?}", self, earliest_retry_after);

        // this might be None
        Err(earliest_retry_after)
    }

    /// get all rpc servers that are not rate limited
    /// returns servers even if they aren't in sync. This is useful for broadcasting signed transactions
    pub async fn get_upstream_servers(&self) -> Result<Vec<ActiveRequestHandle>, Option<Duration>> {
        let mut earliest_retry_after = None;
        // TODO: with capacity?
        let mut selected_rpcs = vec![];

        for connection in self.inner.iter() {
            // check rate limits and increment our connection counter
            match connection.try_request_handle().await {
                Err(retry_after) => {
                    earliest_retry_after = earliest_retry_after.min(Some(retry_after));
                    // this rpc is not available. skip it
                }
                Ok(handle) => selected_rpcs.push(handle),
            }
        }

        if !selected_rpcs.is_empty() {
            return Ok(selected_rpcs);
        }

        // return the earliest retry_after (if no rpcs are synced, this will be None)
        Err(earliest_retry_after)
    }

    /// be sure there is a timeout on this or it might loop forever
    pub async fn try_send_best_upstream_server(
        &self,
        request: JsonRpcRequest,
    ) -> anyhow::Result<JsonRpcForwardedResponse> {
        loop {
            match self.next_upstream_server().await {
                Ok(active_request_handle) => {
                    let response_result = active_request_handle
                        .request(&request.method, &request.params)
                        .await;

                    let response =
                        JsonRpcForwardedResponse::from_response_result(response_result, request.id);

                    if response.error.is_some() {
                        trace!(?response, "Sending error reply",);
                        // errors already sent false to the in_flight_tx
                    } else {
                        trace!(?response, "Sending reply");
                    }

                    return Ok(response);
                }
                Err(None) => {
                    warn!(?self, "No servers in sync!");

                    return Err(anyhow::anyhow!("no servers in sync"));
                }
                Err(Some(retry_after)) => {
                    // TODO: move this to a helper function
                    // sleep (TODO: with a lock?) until our rate limits should be available
                    // TODO: if a server catches up sync while we are waiting, we could stop waiting
                    warn!(?retry_after, "All rate limits exceeded. Sleeping");

                    sleep(retry_after).await;

                    continue;
                }
            }
        }
    }

    pub async fn try_send_all_upstream_servers(
        &self,
        request: JsonRpcRequest,
    ) -> anyhow::Result<JsonRpcForwardedResponse> {
        // TODO: timeout on this loop
        loop {
            match self.get_upstream_servers().await {
                Ok(active_request_handles) => {
                    // TODO: benchmark this compared to waiting on unbounded futures
                    // TODO: do something with this handle?
                    // TODO: this is not working right. simplify
                    let quorum_response = self
                        .try_send_parallel_requests(
                            active_request_handles,
                            request.method.as_ref(),
                            request.params.as_deref(),
                        )
                        .await?;

                    let response = JsonRpcForwardedResponse {
                        jsonrpc: "2.0".to_string(),
                        id: request.id,
                        result: Some(quorum_response),
                        error: None,
                    };

                    return Ok(response);
                }
                Err(None) => {
                    warn!(?self, "No servers in sync!");

                    // TODO: return a 502?
                    // TODO: i don't think this will ever happen
                    return Err(anyhow::anyhow!("no available rpcs!"));
                }
                Err(Some(retry_after)) => {
                    // TODO: move this to a helper function
                    // sleep (TODO: with a lock?) until our rate limits should be available
                    // TODO: if a server catches up sync while we are waiting, we could stop waiting
                    sleep(retry_after).await;

                    warn!("All rate limits exceeded. Sleeping");
                }
            }
        }
    }
}
