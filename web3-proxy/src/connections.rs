///! Load balanced communication with a group of web3 providers
use derive_more::From;
use ethers::prelude::H256;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use governor::clock::{QuantaClock, QuantaInstant};
use governor::NotUntil;
use hashbrown::HashMap;
use left_right::{Absorb, ReadHandleFactory, WriteHandle};
use serde_json::value::RawValue;
use std::cmp;
use std::fmt;
use std::sync::Arc;
use tokio::task;
use tracing::{info, instrument, trace, warn};

use crate::config::Web3ConnectionConfig;
use crate::connection::{ActiveRequestHandle, Web3Connection};

#[derive(Clone, Default)]
pub struct SyncedConnections {
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
    pub fn get_head_block_hash(&self) -> &H256 {
        &self.head_block_hash
    }

    fn update(
        &mut self,
        log: bool,
        new_block_num: u64,
        new_block_hash: H256,
        rpc: Arc<Web3Connection>,
    ) {
        // TODO: double check this logic
        match new_block_num.cmp(&self.head_block_number) {
            cmp::Ordering::Greater => {
                // the rpc's newest block is the new overall best block
                if log {
                    info!("new head {} from {}", new_block_num, rpc);
                }

                self.inner.clear();
                self.inner.push(rpc);

                self.head_block_number = new_block_num;
                self.head_block_hash = new_block_hash;
            }
            cmp::Ordering::Equal => {
                if new_block_hash != self.head_block_hash {
                    // same height, but different chain
                    // TODO: anything else we should do? set some "nextSafeBlockHeight" to delay sending transactions?
                    // TODO: sometimes a node changes its block. if that happens, a new block is probably right behind this one
                    if log {
                        warn!(
                            "chain is forked at #{}! {} has {}. {} rpcs have {}",
                            new_block_num,
                            rpc,
                            new_block_hash,
                            self.inner.len(),
                            self.head_block_hash
                        );
                    }
                    return;
                }

                // do not clear synced_connections.
                // we just want to add this rpc to the end
                self.inner.push(rpc);
            }
            cmp::Ordering::Less => {
                // this isn't the best block in the tier. don't do anything
                return;
            }
        }

        // TODO: better log
        if log {
            trace!("Now synced: {:?}", self.inner);
        }
    }
}

enum SyncedConnectionsOp {
    SyncedConnectionsUpdate(u64, H256, Arc<Web3Connection>),
    SyncedConnectionsCapacity(usize),
}

impl Absorb<SyncedConnectionsOp> for SyncedConnections {
    fn absorb_first(&mut self, operation: &mut SyncedConnectionsOp, _: &Self) {
        match operation {
            SyncedConnectionsOp::SyncedConnectionsUpdate(new_block_number, new_block_hash, rpc) => {
                self.update(true, *new_block_number, *new_block_hash, rpc.clone());
            }
            SyncedConnectionsOp::SyncedConnectionsCapacity(capacity) => {
                self.inner = Vec::with_capacity(*capacity);
            }
        }
    }

    fn absorb_second(&mut self, operation: SyncedConnectionsOp, _: &Self) {
        match operation {
            SyncedConnectionsOp::SyncedConnectionsUpdate(new_block_number, new_block_hash, rpc) => {
                // TODO: disable logging on this one?
                self.update(false, new_block_number, new_block_hash, rpc);
            }
            SyncedConnectionsOp::SyncedConnectionsCapacity(capacity) => {
                self.inner = Vec::with_capacity(capacity);
            }
        }
    }

    fn drop_first(self: Box<Self>) {}

    fn sync_with(&mut self, first: &Self) {
        // TODO: not sure about this
        *self = first.clone()
    }
}

/// A collection of web3 connections. Sends requests either the current best server or all servers.
#[derive(From)]
pub struct Web3Connections {
    inner: Vec<Arc<Web3Connection>>,
    synced_connections_reader: ReadHandleFactory<SyncedConnections>,
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
    #[instrument(name = "try_new_Web3Connections", skip_all)]
    pub async fn try_new(
        chain_id: usize,
        servers: Vec<Web3ConnectionConfig>,
        http_client: Option<reqwest::Client>,
        clock: &QuantaClock,
        subscribe_heads: bool,
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

        let (block_sender, block_receiver) = flume::unbounded();

        let (mut synced_connections_writer, synced_connections_reader) =
            left_right::new::<SyncedConnections, SyncedConnectionsOp>();

        if subscribe_heads {
            for connection in connections.iter() {
                // subscribe to new heads in a spawned future
                // TODO: channel instead. then we can have one future with write access to a left-right?
                let connection = Arc::clone(connection);
                let block_sender = block_sender.clone();
                task::Builder::default()
                    .name("subscribe_new_heads")
                    .spawn(async move {
                        let url = connection.url().to_string();

                        // loop to automatically reconnect
                        // TODO: make this cancellable?
                        loop {
                            // TODO: instead of passing Some(connections), pass Some(channel_sender). Then listen on the receiver below to keep local heads up-to-date
                            if let Err(e) = connection
                                .clone()
                                .subscribe_new_heads(block_sender.clone(), true)
                                .await
                            {
                                warn!("new_heads error on {}: {:?}", url, e);
                            }
                        }
                    });
            }
        }

        synced_connections_writer.append(SyncedConnectionsOp::SyncedConnectionsCapacity(
            num_connections,
        ));
        synced_connections_writer.publish();

        let connections = Arc::new(Self {
            inner: connections,
            synced_connections_reader: synced_connections_reader.factory(),
        });

        if subscribe_heads {
            let connections = Arc::clone(&connections);
            task::Builder::default()
                .name("update_synced_rpcs")
                .spawn(async move {
                    connections
                        .update_synced_rpcs(block_receiver, synced_connections_writer)
                        .await
                });
        }

        Ok(connections)
    }

    pub fn get_synced_rpcs(&self) -> left_right::ReadHandle<SyncedConnections> {
        self.synced_connections_reader.handle()
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
    #[instrument(skip_all)]
    async fn update_synced_rpcs(
        &self,
        block_receiver: flume::Receiver<(u64, H256, Arc<Web3Connection>)>,
        mut synced_connections_writer: WriteHandle<SyncedConnections, SyncedConnectionsOp>,
    ) -> anyhow::Result<()> {
        while let Ok((new_block_num, new_block_hash, rpc)) = block_receiver.recv_async().await {
            if new_block_num == 0 {
                warn!("{} is still syncing", rpc);
                continue;
            }

            synced_connections_writer.append(SyncedConnectionsOp::SyncedConnectionsUpdate(
                new_block_num,
                new_block_hash,
                rpc,
            ));

            // TODO: only publish when the second block arrives?
            synced_connections_writer.publish();
        }

        Ok(())
    }

    /// get the best available rpc server
    #[instrument(skip_all)]
    pub async fn next_upstream_server(
        &self,
    ) -> Result<ActiveRequestHandle, Option<NotUntil<QuantaInstant>>> {
        let mut earliest_not_until = None;

        let mut synced_rpc_arcs = self
            .synced_connections_reader
            .handle()
            .enter()
            .map(|x| x.inner.clone())
            .unwrap();

        // TODO: better key!
        let sort_cache: HashMap<String, (f32, u32)> = synced_rpc_arcs
            .iter()
            .map(|connection| {
                // TODO: better key!
                let key = format!("{}", connection);
                let active_requests = connection.active_requests();
                let soft_limit = connection.soft_limit();

                // TODO: how should we include the soft limit? floats are slower than integer math
                let utilization = active_requests as f32 / soft_limit as f32;

                (key, (utilization, soft_limit))
            })
            .collect();

        // TODO: i think we might need to load active connections and then
        synced_rpc_arcs.sort_unstable_by(|a, b| {
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

        for selected_rpc in synced_rpc_arcs.into_iter() {
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
