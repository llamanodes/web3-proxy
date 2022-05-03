///! Communicate with a group of web3 providers
use arc_swap::ArcSwap;
use derive_more::From;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use fxhash::FxHashMap;
use governor::clock::{QuantaClock, QuantaInstant};
use governor::NotUntil;
use serde_json::value::RawValue;
use std::cmp;
use std::fmt;
use std::sync::Arc;
use tracing::warn;

use crate::config::Web3ConnectionConfig;
use crate::connection::{JsonRpcForwardedResponse, Web3Connection};

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
    /// TODO: arcswap was a lot faster, but i think we need a lock for proper logic
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
    pub async fn try_new(
        // TODO: servers should be a Web3ConnectionBuilder struct
        servers: Vec<Web3ConnectionConfig>,
        http_client: Option<reqwest::Client>,
        clock: &QuantaClock,
    ) -> anyhow::Result<Arc<Self>> {
        let mut connections = vec![];

        let num_connections = servers.len();

        for server_config in servers.into_iter() {
            let connection = server_config.try_build(clock, http_client.clone()).await?;

            connections.push(connection);
        }

        let connections = Arc::new(Self {
            inner: connections,
            synced_connections: ArcSwap::new(Arc::new(SyncedConnections::new(num_connections))),
        });

        for connection in connections.inner.iter() {
            // subscribe to new heads in a spawned future
            // TODO: channel instead. then we can have one future with write access to a left-right
            let connection = Arc::clone(connection);
            let connections = connections.clone();
            tokio::spawn(async move {
                // TODO: instead of passing Some(connections), pass Some(channel_sender). Then listen on the receiver below to keep local heads up-to-date
                if let Err(e) = connection.new_heads(Some(connections)).await {
                    warn!("new_heads error: {:?}", e);
                }
            });
        }

        Ok(connections)
    }

    pub async fn try_send_request(
        &self,
        connection: &Web3Connection,
        method: &str,
        params: &RawValue,
    ) -> anyhow::Result<JsonRpcForwardedResponse> {
        // connection.in_active_requests was called when this rpc was selected

        let response = connection.request(method, params).await;

        connection.dec_active_requests();

        // TODO: if "no block with that header" or some other jsonrpc errors, skip this response

        response.map_err(Into::into)
    }

    pub async fn try_send_requests(
        self: Arc<Self>,
        connections: Vec<Arc<Web3Connection>>,
        method: String,
        params: Box<RawValue>,
        response_sender: flume::Sender<anyhow::Result<JsonRpcForwardedResponse>>,
    ) -> anyhow::Result<()> {
        let mut unordered_futures = FuturesUnordered::new();

        for connection in connections {
            // clone things so we can pass them to a future
            let connections = self.clone();
            let method = method.clone();
            let params = params.clone();
            let response_sender = response_sender.clone();

            let handle = tokio::spawn(async move {
                // get the client for this rpc server
                let response = connections
                    .try_send_request(connection.as_ref(), &method, &params)
                    .await?;

                // send the first good response to a one shot channel. that way we respond quickly
                // drop the result because errors are expected after the first send
                response_sender.send(Ok(response)).map_err(Into::into)
            });

            unordered_futures.push(handle);
        }

        // TODO: use iterators instead of pushing into a vec
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

    pub fn update_synced_rpcs(
        &self,
        rpc: &Arc<Web3Connection>,
        new_block: u64,
    ) -> anyhow::Result<()> {
        // TODO: is RwLock the best type for this? i don't think so anymore. we probably want to use channels and have a single writer using a left-right or arcswap or something
        // TODO: start with a read lock?
        let synced_connections = self.synced_connections.load();

        // should we load new_block here?

        let mut new_synced_connections: SyncedConnections =
            match synced_connections.head_block_number.cmp(&new_block) {
                cmp::Ordering::Equal => {
                    // this rpc is synced, but it isn't the first to this block
                    (**synced_connections).to_owned()
                }
                cmp::Ordering::Less => {
                    // this is a new head block. clear the current synced connections
                    // TODO: this is too verbose with a bunch of tiers. include the tier
                    // info!("new head block from {:?}: {}", rpc, new_block);

                    let mut new_synced_connections = SyncedConnections::new(self.inner.len());

                    // synced_connections.inner.clear();

                    new_synced_connections.head_block_number = new_block;

                    new_synced_connections
                }
                cmp::Ordering::Greater => {
                    // not the latest block. return now
                    return Ok(());
                }
            };

        let rpc_index = self
            .inner
            .iter()
            .position(|x| x.url() == rpc.url())
            .unwrap();

        new_synced_connections.inner.push(rpc_index);

        self.synced_connections
            .swap(Arc::new(new_synced_connections));

        Ok(())
    }

    /// get the best available rpc server
    pub async fn next_upstream_server(
        &self,
    ) -> Result<Arc<Web3Connection>, Option<NotUntil<QuantaInstant>>> {
        let mut earliest_not_until = None;

        // TODO: this clone is probably not the best way to do this
        let mut synced_rpc_indexes = self.synced_connections.load().inner.clone();

        let cache: FxHashMap<usize, u32> = synced_rpc_indexes
            .iter()
            .map(|synced_index| {
                (
                    *synced_index,
                    self.inner.get(*synced_index).unwrap().active_requests(),
                )
            })
            .collect();

        // TODO: i think we might need to load active connections and then
        synced_rpc_indexes.sort_unstable_by(|a, b| {
            let a = cache.get(a).unwrap();
            let b = cache.get(b).unwrap();

            a.cmp(b)
        });

        for selected_rpc in synced_rpc_indexes.into_iter() {
            let selected_rpc = self.inner.get(selected_rpc).unwrap();

            // increment our connection counter
            if let Err(not_until) = selected_rpc.try_inc_active_requests() {
                earliest_possible(&mut earliest_not_until, not_until);

                continue;
            }

            // return the selected RPC
            return Ok(selected_rpc.clone());
        }

        // this might be None
        Err(earliest_not_until)
    }

    /// get all rpc servers that are not rate limited
    /// even fetches if they aren't in sync. This is useful for broadcasting signed transactions
    pub fn get_upstream_servers(
        &self,
    ) -> Result<Vec<Arc<Web3Connection>>, Option<NotUntil<QuantaInstant>>> {
        let mut earliest_not_until = None;
        // TODO: with capacity?
        let mut selected_rpcs = vec![];

        for connection in self.inner.iter() {
            // check rate limits and increment our connection counter
            if let Err(not_until) = connection.try_inc_active_requests() {
                earliest_possible(&mut earliest_not_until, not_until);

                // this rpc is not available. skip it
                continue;
            }

            selected_rpcs.push(connection.clone());
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
