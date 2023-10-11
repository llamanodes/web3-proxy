//! Load balanced communication with a group of web3 rpc providers
use super::blockchain::{BlocksByHashCache, BlocksByNumberCache, Web3ProxyBlock};
use super::consensus::{RankedRpcs, RpcsForRequest};
use super::one::Web3Rpc;
use crate::app::{flatten_handle, App, Web3ProxyJoinHandle};
use crate::config::{average_block_interval, BlockAndRpc, Web3RpcConfig};
use crate::errors::{Web3ProxyError, Web3ProxyResult};
use crate::frontend::rpc_proxy_ws::ProxyMode;
use crate::frontend::status::MokaCacheSerializer;
use crate::jsonrpc::ValidatedRequest;
use crate::jsonrpc::{self, JsonRpcErrorData, JsonRpcParams, JsonRpcResultData};
use deduped_broadcast::DedupedBroadcaster;
use derive_more::From;
use ethers::prelude::{TxHash, U64};
use futures::future::try_join_all;
use futures::stream::StreamExt;
use futures_util::future::join_all;
use hashbrown::HashMap;
use http::StatusCode;
use moka::future::CacheBuilder;
use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;
use serde_json::json;
use std::borrow::Cow;
use std::fmt::{self, Display};
use std::sync::Arc;
use tokio::sync::{mpsc, watch, RwLock as AsyncRwLock};
use tokio::time::{sleep_until, Duration, Instant};
use tokio::{pin, select};
use tracing::{debug, error, info, trace, warn};

/// A collection of web3 connections. Sends requests either the current best server or all servers.
#[derive(From)]
pub struct Web3Rpcs {
    pub(crate) name: Cow<'static, str>,
    pub(crate) chain_id: u64,
    /// if watch_consensus_head_sender is some, Web3Rpc inside self will send blocks here when they get them
    pub(crate) block_sender: mpsc::UnboundedSender<(Option<Web3ProxyBlock>, Arc<Web3Rpc>)>,
    /// any requests will be forwarded to one (or more) of these connections
    /// TODO: hopefully this not being an async lock will be okay. if you need it across awaits, clone the arc
    pub(crate) by_name: AsyncRwLock<HashMap<String, Arc<Web3Rpc>>>,
    /// all providers with the same consensus head block. won't update if there is no `self.watch_consensus_head_sender`
    /// TODO: document that this is a watch sender and not a broadcast! if things get busy, blocks might get missed
    /// TODO: why is watch_consensus_head_sender in an Option, but this one isn't?
    /// Geth's subscriptions have the same potential for skipping blocks.
    pub(crate) watch_ranked_rpcs: watch::Sender<Option<Arc<RankedRpcs>>>,
    /// this head receiver makes it easy to wait until there is a new block
    pub(super) watch_head_block: Option<watch::Sender<Option<Web3ProxyBlock>>>,
    /// TODO: this map is going to grow forever unless we do some sort of pruning. maybe store pruned in redis?
    /// all blocks, including uncles
    /// TODO: i think uncles should be excluded
    pub(crate) blocks_by_hash: BlocksByHashCache,
    /// blocks on the heaviest chain
    pub(crate) blocks_by_number: BlocksByNumberCache,
    /// the number of rpcs required to agree on consensus for the head block (thundering herd protection)
    pub(super) min_synced_rpcs: usize,
    /// the soft limit required to agree on consensus for the head block. (thundering herd protection)
    pub(super) min_sum_soft_limit: u32,
    /// how far behind the highest known block height we can be before we stop serving requests
    pub(super) max_head_block_lag: U64,
    /// how old our consensus head block we can be before we stop serving requests
    /// calculated based on max_head_block_lag and averge block times
    pub(super) max_head_block_age: Duration,
    /// all of the pending txids for all of the rpcs. this still has duplicates
    pub(super) pending_txid_firehose: Option<Arc<DedupedBroadcaster<TxHash>>>,
}

/// this is a RankedRpcs that should be ready to use
/// there is a small chance of waiting for rate limiting depending on how many backend servers you have
#[derive(From)]
pub enum TryRpcsForRequest {
    Some(RpcsForRequest),
    RetryAt(Instant),
    // WaitForBlock(U64),
    None,
}

impl From<Option<Instant>> for TryRpcsForRequest {
    fn from(value: Option<Instant>) -> Self {
        match value {
            None => Self::None,
            Some(x) => Self::RetryAt(x),
        }
    }
}

impl From<Option<RpcsForRequest>> for TryRpcsForRequest {
    fn from(value: Option<RpcsForRequest>) -> Self {
        match value {
            None => Self::None,
            Some(x) => Self::Some(x),
        }
    }
}

impl Web3Rpcs {
    /// Spawn durable connections to multiple Web3 providers.
    pub async fn spawn(
        chain_id: u64,
        max_head_block_lag: Option<U64>,
        min_head_rpcs: usize,
        min_sum_soft_limit: u32,
        name: Cow<'static, str>,
        watch_consensus_head_sender: Option<watch::Sender<Option<Web3ProxyBlock>>>,
        pending_txid_firehose: Option<Arc<DedupedBroadcaster<TxHash>>>,
    ) -> anyhow::Result<(
        Arc<Self>,
        Web3ProxyJoinHandle<()>,
        watch::Receiver<Option<Arc<RankedRpcs>>>,
    )> {
        let (block_and_rpc_sender, block_and_rpc_receiver) =
            mpsc::unbounded_channel::<BlockAndRpc>();

        // these blocks don't have full transactions, but they do have rather variable amounts of transaction hashes
        // TODO: actual weighter on this
        // TODO: time_to_idle instead?
        let blocks_by_hash: BlocksByHashCache = CacheBuilder::new(10_000)
            .name("blocks_by_hash")
            .time_to_idle(Duration::from_secs(30 * 60))
            .build();

        // all block numbers are the same size, so no need for weigher
        // TODO: limits from config
        // TODO: time_to_idle instead?
        let blocks_by_number = CacheBuilder::new(10_000)
            .name("blocks_by_number")
            .time_to_idle(Duration::from_secs(30 * 60))
            .build();

        let (watch_consensus_rpcs_sender, consensus_connections_watcher) =
            watch::channel(Default::default());

        // by_name starts empty. self.apply_server_configs will add to it
        let by_name = AsyncRwLock::new(HashMap::new());

        let max_head_block_lag = max_head_block_lag.unwrap_or(5.into());

        let max_head_block_age =
            average_block_interval(chain_id).mul_f32((max_head_block_lag.as_u64() * 10) as f32);

        let connections = Arc::new(Self {
            block_sender: block_and_rpc_sender,
            blocks_by_hash,
            blocks_by_number,
            by_name,
            chain_id,
            max_head_block_age,
            max_head_block_lag,
            min_synced_rpcs: min_head_rpcs,
            min_sum_soft_limit,
            name,
            pending_txid_firehose,
            watch_head_block: watch_consensus_head_sender,
            watch_ranked_rpcs: watch_consensus_rpcs_sender,
        });

        let handle = {
            let connections = connections.clone();

            tokio::spawn(connections.subscribe(block_and_rpc_receiver))
        };

        Ok((connections, handle, consensus_connections_watcher))
    }

    /// update the rpcs in this group
    pub async fn apply_server_configs(
        &self,
        app: &App,
        rpc_configs: &HashMap<String, Web3RpcConfig>,
    ) -> Web3ProxyResult<()> {
        // safety checks
        if rpc_configs.len() < app.config.min_synced_rpcs {
            // TODO: don't count disabled servers!
            // TODO: include if this is balanced, private, or 4337
            warn!(
                "Only {}/{} rpcs! Add more rpcs or reduce min_synced_rpcs.",
                rpc_configs.len(),
                app.config.min_synced_rpcs
            );
            return Ok(());
        }

        // safety check on sum soft limit
        // TODO: will need to think about this more once sum_soft_limit is dynamic
        let sum_soft_limit = rpc_configs.values().fold(0, |acc, x| acc + x.soft_limit);

        // TODO: require a buffer?
        if sum_soft_limit < self.min_sum_soft_limit {
            return Err(Web3ProxyError::NotEnoughSoftLimit {
                available: sum_soft_limit,
                needed: self.min_sum_soft_limit,
            });
        }

        let chain_id = app.config.chain_id;

        let block_interval = average_block_interval(chain_id);

        let mut names_to_keep = vec![];

        let server_id = app.config.unique_id;

        // turn configs into connections (in parallel)
        let spawn_handles: Vec<_> = rpc_configs
            .into_iter()
            .filter_map(|(server_name, server_config)| {
                if server_config.disabled {
                    info!("{} is disabled", server_name);
                    return None;
                }

                let http_client = app.http_client.clone();
                let vredis_pool = app.vredis_pool.clone();

                let block_and_rpc_sender = if self.watch_head_block.is_some() {
                    Some(self.block_sender.clone())
                } else {
                    None
                };

                let blocks_by_hash_cache = self.blocks_by_hash.clone();

                debug!("spawning tasks for {}", server_name);

                names_to_keep.push(server_name.clone());

                let handle = server_config.clone().spawn(
                    server_name.clone(),
                    vredis_pool,
                    server_id,
                    chain_id,
                    block_interval,
                    http_client,
                    blocks_by_hash_cache,
                    block_and_rpc_sender,
                    self.pending_txid_firehose.clone(),
                    self.max_head_block_age,
                );

                Some(handle)
            })
            .collect();

        for x in join_all(spawn_handles).await {
            match x {
                Ok((new_rpc, _handle)) => {
                    // web3 connection worked

                    // we don't remove it yet because we want the new one connected first
                    let old_rpc = {
                        let by_name = self.by_name.read().await;
                        by_name.get(&new_rpc.name).cloned()
                    };

                    // clean up the old rpc if it exists
                    if let Some(old_rpc) = old_rpc {
                        trace!("old_rpc: {}", old_rpc);

                        // if the old rpc was synced, wait for the new one to sync
                        if old_rpc
                            .head_block_sender
                            .as_ref()
                            .unwrap()
                            .borrow()
                            .is_some()
                        {
                            let mut new_head_receiver =
                                new_rpc.head_block_sender.as_ref().unwrap().subscribe();
                            trace!("waiting for new {} connection to sync", new_rpc);

                            // TODO: maximum wait time
                            while new_head_receiver.borrow_and_update().is_none() {
                                if new_head_receiver.changed().await.is_err() {
                                    break;
                                };
                            }
                        }

                        // new rpc is synced (or old one was not synced). update the local map
                        // make sure that any new requests use the new connection
                        self.by_name
                            .write()
                            .await
                            .insert(new_rpc.name.clone(), new_rpc);

                        // tell the old rpc to disconnect
                        if let Some(ref disconnect_sender) = old_rpc.disconnect_watch {
                            debug!("telling old {} to disconnect", old_rpc);
                            disconnect_sender.send_replace(true);
                        }
                    } else {
                        self.by_name
                            .write()
                            .await
                            .insert(new_rpc.name.clone(), new_rpc);
                    }
                }
                Err(err) => {
                    // if we got an error here, the app can continue on
                    // TODO: include context about which connection failed
                    // TODO: retry automatically
                    error!("Unable to create connection. err={:?}", err);
                }
            }
        }

        // TODO: remove any RPCs that were part of the config, but are now removed
        let active_names: Vec<_> = self.by_name.read().await.keys().cloned().collect();
        for name in active_names {
            if names_to_keep.contains(&name) {
                continue;
            }
            if let Some(old_rpc) = self.by_name.write().await.remove(&name) {
                if let Some(ref disconnect_sender) = old_rpc.disconnect_watch {
                    debug!("telling {} to disconnect. no longer needed", old_rpc);
                    disconnect_sender.send_replace(true);
                }
            }
        }

        let num_rpcs = self.len().await;

        if num_rpcs < self.min_synced_rpcs {
            return Err(Web3ProxyError::NotEnoughRpcs {
                num_known: num_rpcs,
                min_head_rpcs: self.min_synced_rpcs,
            });
        }

        Ok(())
    }

    pub async fn get(&self, conn_name: &str) -> Option<Arc<Web3Rpc>> {
        self.by_name.read().await.get(conn_name).cloned()
    }

    pub async fn len(&self) -> usize {
        self.by_name.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.by_name.read().await.is_empty()
    }

    /// TODO: rename to be consistent between "head" and "synced"
    pub fn min_head_rpcs(&self) -> usize {
        self.min_synced_rpcs
    }

    /// subscribe to blocks and transactions from all the backend rpcs.
    /// blocks are processed by all the `Web3Rpc`s and then sent to the `block_receiver`
    /// transaction ids from all the `Web3Rpc`s are deduplicated and forwarded to `pending_tx_sender`
    async fn subscribe(
        self: Arc<Self>,
        block_and_rpc_receiver: mpsc::UnboundedReceiver<BlockAndRpc>,
    ) -> Web3ProxyResult<()> {
        let mut futures = vec![];

        // TODO: do we need anything here to set up the transaction funnel

        // setup the block funnel
        if self.watch_head_block.is_some() {
            let connections = Arc::clone(&self);

            let handle = tokio::task::Builder::default()
                .name("process_incoming_blocks")
                .spawn(async move {
                    connections
                        .process_incoming_blocks(block_and_rpc_receiver)
                        .await
                })?;

            futures.push(flatten_handle(handle));
        }

        if futures.is_empty() {
            // no transaction or block subscriptions.

            // TODO: i don't like this. it's a hack to keep the tokio task alive
            let handle = tokio::task::Builder::default()
                .name("noop")
                .spawn(async move {
                    futures::future::pending::<()>().await;

                    Ok(())
                })?;

            futures.push(flatten_handle(handle));
        }

        if let Err(e) = try_join_all(futures).await {
            error!(?self, "subscriptions over");
            return Err(e);
        }

        info!(?self, "subscriptions over");
        Ok(())
    }

    /// TODO: i think this RpcsForRequest should be stored on the ValidatedRequest when its made. that way any waiting for sync happens early and we don't need waiting anywhere else in the app
    pub async fn wait_for_rpcs_for_request(
        &self,
        web3_request: &Arc<ValidatedRequest>,
    ) -> Web3ProxyResult<RpcsForRequest> {
        let mut watch_consensus_rpcs = self.watch_ranked_rpcs.subscribe();

        loop {
            // other places check web3_request ttl. i don't think we need a check here too
            let next_try = match self.try_rpcs_for_request(web3_request).await {
                Ok(x) => return Ok(x),
                Err(Web3ProxyError::RateLimited(_, Some(retry_at))) => retry_at,
                Err(x) => return Err(x),
            };

            if next_try > web3_request.connect_timeout_at() {
                let retry_in = Instant::now().duration_since(next_try).as_secs().min(1);

                // we don't use Web3ProxyError::RateLimited because that is for the user being rate limited
                return Err(Web3ProxyError::StatusCode(
                    StatusCode::TOO_MANY_REQUESTS,
                    "backend rpcs are all rate limited!".into(),
                    Some(json!({"retry_in": retry_in})),
                ));
            }

            trace!(?next_try, "retry needed");

            // todo!("this must be a bug in our tests. in prod if things are overloaded i could see it happening")
            debug_assert!(Instant::now() < next_try);

            select! {
                _ = sleep_until(next_try) => {
                    // rpcs didn't change and we have waited too long. break to return an error
                    warn!(?self, "timeout during wait_for_rpcs_for_request!");
                },
                _ = watch_consensus_rpcs.changed() => {
                    // consensus rpcs changed!
                    // TODO: i don't love that we throw it away
                    watch_consensus_rpcs.borrow_and_update();
                }
            }
        }
    }

    /// get all rpc servers that are not rate limited
    /// this prefers synced servers, but it will return servers even if they aren't fully in sync.
    /// this does not gaurentee you won't be rate limited. we don't increment our counters until you try to send. so you might have to wait to be able to send
    /// TODO: should this wait for ranked rpcs? maybe only a fraction of web3_request's time?
    pub async fn try_rpcs_for_request(
        &self,
        web3_request: &Arc<ValidatedRequest>,
    ) -> Web3ProxyResult<RpcsForRequest> {
        // TODO: by_name might include things that are on a forked

        let option_ranked_rpcs = self.watch_ranked_rpcs.borrow().clone();

        let ranked_rpcs: Arc<RankedRpcs> = if let Some(ranked_rpcs) = option_ranked_rpcs {
            ranked_rpcs
        } else if self.watch_head_block.is_some() {
            // if we are here, this set of rpcs is subscribed to newHeads. But we didn't get a RankedRpcs. that means something is wrong
            return Err(Web3ProxyError::NoServersSynced);
        } else {
            // no RankedRpcs, but also no newHeads subscription. This is probably a set of "protected" rpcs or similar
            // TODO: return a future that resolves once we do have something?
            let rpcs = {
                let by_name = self.by_name.read().await;
                by_name.values().cloned().collect()
            };

            if let Some(x) = RankedRpcs::from_rpcs(rpcs, web3_request.head_block.clone()) {
                Arc::new(x)
            } else {
                // i doubt we will ever get here
                // TODO: return a future that resolves once we do have something?
                return Err(Web3ProxyError::NoServersSynced);
            }
        };

        match ranked_rpcs.for_request(web3_request) {
            None => Err(Web3ProxyError::NoServersSynced),
            Some(x) => Ok(x),
        }
    }

    pub async fn internal_request<P: JsonRpcParams, R: JsonRpcResultData>(
        &self,
        method: Cow<'static, str>,
        params: &P,
        max_wait: Option<Duration>,
    ) -> Web3ProxyResult<R> {
        let head_block = self.head_block();

        let web3_request =
            ValidatedRequest::new_internal(method, params, head_block, max_wait).await?;

        let response = self.request_with_metadata(&web3_request).await?;

        // the response might support streaming. we need to parse it
        let parsed = response.parsed().await?;

        match parsed.payload {
            jsonrpc::ResponsePayload::Success { result } => Ok(result),
            // TODO: confirm this error type is correct
            jsonrpc::ResponsePayload::Error { error } => Err(error.into()),
        }
    }

    /// Make a request with stat tracking.
    /// The first jsonrpc response will be returned.
    /// TODO? move this to RankedRpcsForRequest along with a bunch of other similar functions? but it needs watch_ranked_rpcs and other things on Web3Rpcs...
    /// TODO: have a similar function for quorum(size, max_tries)
    /// TODO: should max_tries be on web3_request. maybe as tries_left?
    pub async fn request_with_metadata<R: JsonRpcResultData>(
        &self,
        web3_request: &Arc<ValidatedRequest>,
    ) -> Web3ProxyResult<jsonrpc::SingleResponse<R>> {
        // TODO: collect the most common error. Web3ProxyError isn't Hash + Eq though. And making it so would be a pain
        let mut errors = vec![];

        // TODO: limit number of tries
        let rpcs = self.try_rpcs_for_request(web3_request).await?;

        let stream = rpcs.to_stream();

        pin!(stream);

        while let Some(active_request_handle) = stream.next().await {
            // TODO: i'd like to get rid of this clone
            let rpc = active_request_handle.clone_connection();

            web3_request.backend_requests.lock().push(rpc);

            match active_request_handle.request::<R>().await {
                Ok(response) => {
                    return Ok(response);
                }
                Err(error) => {
                    // TODO: if this is an error, do NOT return. continue to try on another server

                    // TODO: track the most common errors
                    errors.push(error);
                }
            }
        }

        if let Some(err) = errors.into_iter().next() {
            return Err(err);
        }

        // let min_block_needed = web3_request.min_block_needed();
        // let max_block_needed = web3_request.max_block_needed();

        // let num_conns = self.len();
        // let num_skipped = skip_rpcs.len();

        // let head_block_num = watch_consensus_rpcs
        //     .borrow_and_update()
        //     .as_ref()
        //     .map(|x| x.head_block.number());

        // // TODO: error? warn? debug? trace?
        // if head_block_num.is_none() {
        //     error!(
        //         min=?min_block_needed,
        //         max=?max_block_needed,
        //         head=?head_block_num,
        //         known=num_conns,
        //         method=%web3_request.request.method(),
        //         params=?web3_request.request.params(),
        //         "No servers synced",
        //     );
        // } else if head_block_num > max_block_needed {
        //     // we have synced past the needed block
        //     // TODO: log ranked rpcs
        //     // TODO: only log params in development
        //     error!(
        //         min=?min_block_needed,
        //         max=?max_block_needed,
        //         head=?head_block_num,
        //         known=%num_conns,
        //         method=%web3_request.request.method(),
        //         params=?web3_request.request.params(),
        //         "No archive servers synced",
        //     );
        // } else {
        //     // TODO: only log params in development
        //     // TODO: log ranked rpcs
        //     error!(
        //         min=?min_block_needed,
        //         max=?max_block_needed,
        //         head=?head_block_num,
        //         skipped=%num_skipped,
        //         known=%num_conns,
        //         method=%web3_request.request.method(),
        //         params=?web3_request.request.params(),
        //         "Requested data is not available",
        //     );
        // }

        // TODO: what error code? what data?
        // cloudflare gives {"jsonrpc":"2.0","error":{"code":-32043,"message":"Requested data cannot be older than 128 blocks."},"id":1}
        Err(JsonRpcErrorData {
            message: "Requested data is not available".into(),
            code: -32001,
            data: Some(json!({
                "request": web3_request
            })),
        }
        .into())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn try_proxy_connection<R: JsonRpcResultData>(
        &self,
        web3_request: &Arc<ValidatedRequest>,
    ) -> Web3ProxyResult<jsonrpc::SingleResponse<R>> {
        let proxy_mode = web3_request.proxy_mode();

        match proxy_mode {
            ProxyMode::Debug | ProxyMode::Best => self.request_with_metadata(web3_request).await,
            ProxyMode::Fastest(_x) => todo!("Fastest"),
            ProxyMode::Quorum(_x, _y) => todo!("Quorum"),
            ProxyMode::Versus => todo!("Versus"),
        }
    }
}

impl Display for Web3Rpcs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}

impl fmt::Debug for Web3Rpcs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        let consensus_rpcs = self.watch_ranked_rpcs.borrow().is_some();

        // let names = self.by_name.blocking_read();
        // let names = names.values().map(|x| x.name.as_str()).collect::<Vec<_>>();
        let names = ();

        let head_block = self.head_block();

        f.debug_struct("Web3Rpcs")
            .field("rpcs", &names)
            .field("consensus_rpcs", &consensus_rpcs)
            .field("head_block", &head_block)
            .finish_non_exhaustive()
    }
}

impl Serialize for Web3Rpcs {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("Web3Rpcs", 5)?;

        {
            // let by_name = self.by_name.read().await;
            // let rpcs: Vec<&Arc<Web3Rpc>> = by_name.values().collect();
            // TODO: coordinate with frontend team to rename "conns" to "rpcs"
            let rpcs = ();
            state.serialize_field("conns", &rpcs)?;
        }

        {
            let consensus_rpcs = self.watch_ranked_rpcs.borrow().clone();
            // TODO: rename synced_connections to consensus_rpcs

            if let Some(consensus_rpcs) = consensus_rpcs.as_ref() {
                state.serialize_field("synced_connections", consensus_rpcs)?;
            } else {
                state.serialize_field("synced_connections", &None::<()>)?;
            }
        }

        state.serialize_field(
            "caches",
            &(
                MokaCacheSerializer(&self.blocks_by_hash),
                MokaCacheSerializer(&self.blocks_by_number),
            ),
        )?;

        state.serialize_field(
            "watch_consensus_rpcs_receivers",
            &self.watch_ranked_rpcs.receiver_count(),
        )?;

        if let Some(ref x) = self.watch_head_block {
            state.serialize_field("watch_consensus_head_receivers", &x.receiver_count())?;
        } else {
            state.serialize_field("watch_consensus_head_receivers", &None::<()>)?;
        }

        state.end()
    }
}

mod tests {
    #![allow(unused_imports)]

    use super::*;
    use crate::block_number::{BlockNumAndHash, CacheMode};
    use crate::rpcs::blockchain::Web3ProxyBlock;
    use crate::rpcs::consensus::ConsensusFinder;
    use arc_swap::ArcSwap;
    use ethers::types::H256;
    use ethers::types::{Block, U256};
    use latency::PeakEwmaLatency;
    use moka::future::{Cache, CacheBuilder};
    use std::cmp::Reverse;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tracing::trace;

    #[cfg(test)]
    fn new_peak_latency() -> PeakEwmaLatency {
        PeakEwmaLatency::spawn(Duration::from_secs(1), 4, Duration::from_secs(1))
    }

    #[test_log::test(tokio::test)]
    async fn test_sort_connections_by_sync_status() {
        let block_0 = Block {
            number: Some(0.into()),
            hash: Some(H256::random()),
            ..Default::default()
        };
        let block_1 = Block {
            number: Some(1.into()),
            hash: Some(H256::random()),
            parent_hash: block_0.hash.unwrap(),
            ..Default::default()
        };
        let block_2 = Block {
            number: Some(2.into()),
            hash: Some(H256::random()),
            parent_hash: block_1.hash.unwrap(),
            ..Default::default()
        };

        let blocks: Vec<_> = [block_0, block_1, block_2]
            .into_iter()
            .map(|x| Web3ProxyBlock::try_new(Arc::new(x)).unwrap())
            .collect();

        let (tx_a, _) = watch::channel(None);
        let (tx_b, _) = watch::channel(blocks.get(1).cloned());
        let (tx_c, _) = watch::channel(blocks.get(2).cloned());
        let (tx_d, _) = watch::channel(None);
        let (tx_e, _) = watch::channel(blocks.get(1).cloned());
        let (tx_f, _) = watch::channel(blocks.get(2).cloned());

        let mut rpcs: Vec<_> = [
            Web3Rpc {
                name: "a".to_string(),
                tier: 0.into(),
                head_block_sender: Some(tx_a),
                peak_latency: Some(new_peak_latency()),
                ..Default::default()
            },
            Web3Rpc {
                name: "b".to_string(),
                tier: 0.into(),
                head_block_sender: Some(tx_b),
                peak_latency: Some(new_peak_latency()),
                ..Default::default()
            },
            Web3Rpc {
                name: "c".to_string(),
                tier: 0.into(),
                head_block_sender: Some(tx_c),
                peak_latency: Some(new_peak_latency()),
                ..Default::default()
            },
            Web3Rpc {
                name: "d".to_string(),
                tier: 1.into(),
                head_block_sender: Some(tx_d),
                peak_latency: Some(new_peak_latency()),
                ..Default::default()
            },
            Web3Rpc {
                name: "e".to_string(),
                tier: 1.into(),
                head_block_sender: Some(tx_e),
                peak_latency: Some(new_peak_latency()),
                ..Default::default()
            },
            Web3Rpc {
                name: "f".to_string(),
                tier: 1.into(),
                head_block_sender: Some(tx_f),
                peak_latency: Some(new_peak_latency()),
                ..Default::default()
            },
        ]
        .into_iter()
        .map(Arc::new)
        .collect();

        let now = Instant::now();

        // TODO: also test with max_block = 0 and 1
        rpcs.sort_by_cached_key(|x| x.sort_for_load_balancing_on(Some(2.into()), now));

        let names_in_sort_order: Vec<_> = rpcs.iter().map(|x| x.name.as_str()).collect();

        // assert_eq!(names_in_sort_order, ["c", "b", "a", "f", "e", "d"]);
        assert_eq!(names_in_sort_order, ["c", "f", "b", "e", "a", "d"]);
    }

    // #[test_log::test(tokio::test)]
    // async fn test_server_selection_by_height() {
    //     let now = chrono::Utc::now().timestamp().into();

    //     let lagged_block = Block {
    //         hash: Some(H256::random()),
    //         number: Some(0.into()),
    //         timestamp: now - 1,
    //         ..Default::default()
    //     };

    //     let head_block = Block {
    //         hash: Some(H256::random()),
    //         number: Some(1.into()),
    //         parent_hash: lagged_block.hash.unwrap(),
    //         timestamp: now,
    //         ..Default::default()
    //     };

    //     let lagged_block = Arc::new(lagged_block);
    //     let head_block = Arc::new(head_block);

    //     let block_data_limit = u64::MAX;

    //     let (tx_synced, _) = watch::channel(None);

    //     let head_rpc = Web3Rpc {
    //         name: "synced".to_string(),
    //         soft_limit: 1_000,
    //         automatic_block_limit: false,
    //         backup: false,
    //         block_data_limit: block_data_limit.into(),
    //         head_block_sender: Some(tx_synced),
    //         peak_latency: Some(new_peak_latency()),
    //         ..Default::default()
    //     };

    //     let (tx_lagged, _) = watch::channel(None);

    //     let lagged_rpc = Web3Rpc {
    //         name: "lagged".to_string(),
    //         soft_limit: 1_000,
    //         automatic_block_limit: false,
    //         backup: false,
    //         block_data_limit: block_data_limit.into(),
    //         head_block_sender: Some(tx_lagged),
    //         peak_latency: Some(new_peak_latency()),
    //         ..Default::default()
    //     };

    //     assert!(!head_rpc.has_block_data(lagged_block.number.unwrap()));
    //     assert!(!head_rpc.has_block_data(head_block.number.unwrap()));

    //     assert!(!lagged_rpc.has_block_data(lagged_block.number.unwrap()));
    //     assert!(!lagged_rpc.has_block_data(head_block.number.unwrap()));

    //     let head_rpc = Arc::new(head_rpc);
    //     let lagged_rpc = Arc::new(lagged_rpc);

    //     let (block_sender, _block_receiver) = mpsc::unbounded_channel();
    //     let (watch_ranked_rpcs, _watch_consensus_rpcs_receiver) = watch::channel(None);
    //     let (watch_consensus_head_sender, _watch_consensus_head_receiver) = watch::channel(None);

    //     let chain_id = 1;

    //     let mut by_name = HashMap::new();
    //     by_name.insert(head_rpc.name.clone(), head_rpc.clone());
    //     by_name.insert(lagged_rpc.name.clone(), lagged_rpc.clone());

    //     // TODO: make a Web3Rpcs::new
    //     let rpcs = Web3Rpcs {
    //         block_sender: block_sender.clone(),
    //         by_name: RwLock::new(by_name),
    //         chain_id,
    //         name: "test".into(),
    //         watch_head_block: Some(watch_consensus_head_sender),
    //         watch_ranked_rpcs,
    //         blocks_by_hash: CacheBuilder::new(100)
    //             .time_to_live(Duration::from_secs(60))
    //             .build(),
    //         blocks_by_number: CacheBuilder::new(100)
    //             .time_to_live(Duration::from_secs(60))
    //             .build(),
    //         // TODO: test max_head_block_age?
    //         max_head_block_age: Duration::from_secs(60),
    //         // TODO: test max_head_block_lag?
    //         max_head_block_lag: 5.into(),
    //         pending_txid_firehose_sender: None,
    //         min_synced_rpcs: 1,
    //         min_sum_soft_limit: 1,
    //     };

    //     let mut consensus_finder = ConsensusFinder::new(None, None);

    //     consensus_finder
    //         .process_block_from_rpc(&rpcs, None, lagged_rpc.clone())
    //         .await
    //         .expect(
    //             "its lagged, but it should still be seen as consensus if its the first to report",
    //         );
    //     consensus_finder
    //         .process_block_from_rpc(&rpcs, None, head_rpc.clone())
    //         .await
    //         .unwrap();

    //     // no head block because the rpcs haven't communicated through their channels
    //     assert!(rpcs.head_block_hash().is_none());

    //     // request that requires the head block
    //     // best_synced_backend_connection which servers to be synced with the head block should not find any nodes
    //     let r = ValidatedRequest::new_internal(
    //         "eth_getBlockByNumber".to_string(),
    //         &(head_block.number.unwrap(), false),
    //         Some(Web3ProxyBlock::try_from(head_block.clone()).unwrap()),
    //         Some(Duration::from_millis(100)),
    //     )
    //     .await
    //     .unwrap();
    //     let x = rpcs
    //         .wait_for_best_rpc(&r, &mut vec![], Some(RequestErrorHandler::DebugLevel))
    //         .await
    //         .unwrap();
    //     info!(?x);
    //     assert!(matches!(x, OpenRequestResult::NotReady));

    //     // add lagged blocks to the rpcs. both servers should be allowed
    //     lagged_rpc
    //         .send_head_block_result(
    //             Ok(Some(lagged_block.clone())),
    //             &block_sender,
    //             &rpcs.blocks_by_hash,
    //         )
    //         .await
    //         .unwrap();

    //     // TODO: calling process_block_from_rpc and send_head_block_result seperate seems very fragile
    //     consensus_finder
    //         .process_block_from_rpc(
    //             &rpcs,
    //             Some(lagged_block.clone().try_into().unwrap()),
    //             lagged_rpc.clone(),
    //         )
    //         .await
    //         .unwrap();

    //     head_rpc
    //         .send_head_block_result(
    //             Ok(Some(lagged_block.clone())),
    //             &block_sender,
    //             &rpcs.blocks_by_hash,
    //         )
    //         .await
    //         .unwrap();

    //     // TODO: this is fragile
    //     consensus_finder
    //         .process_block_from_rpc(
    //             &rpcs,
    //             Some(lagged_block.clone().try_into().unwrap()),
    //             head_rpc.clone(),
    //         )
    //         .await
    //         .unwrap();

    //     // TODO: how do we spawn this and wait for it to process things? subscribe and watch consensus connections?
    //     // rpcs.process_incoming_blocks(block_receiver, pending_tx_sender)

    //     assert!(head_rpc.has_block_data(lagged_block.number.unwrap()));
    //     assert!(!head_rpc.has_block_data(head_block.number.unwrap()));

    //     assert!(lagged_rpc.has_block_data(lagged_block.number.unwrap()));
    //     assert!(!lagged_rpc.has_block_data(head_block.number.unwrap()));

    //     assert_eq!(rpcs.num_synced_rpcs(), 2);

    //     // TODO: tests on all_synced_connections

    //     // add head block to the rpcs. lagged_rpc should not be available
    //     head_rpc
    //         .send_head_block_result(
    //             Ok(Some(head_block.clone())),
    //             &block_sender,
    //             &rpcs.blocks_by_hash,
    //         )
    //         .await
    //         .unwrap();

    //     // TODO: this is fragile
    //     consensus_finder
    //         .process_block_from_rpc(
    //             &rpcs,
    //             Some(head_block.clone().try_into().unwrap()),
    //             head_rpc.clone(),
    //         )
    //         .await
    //         .unwrap();

    //     assert_eq!(rpcs.num_synced_rpcs(), 1);

    //     assert!(head_rpc.has_block_data(lagged_block.number.unwrap()));
    //     assert!(head_rpc.has_block_data(head_block.number.unwrap()));

    //     assert!(lagged_rpc.has_block_data(lagged_block.number.unwrap()));
    //     assert!(!lagged_rpc.has_block_data(head_block.number.unwrap()));

    //     // request on the lagged block should get a handle from either server
    //     let r = ValidatedRequest::new_internal(
    //         "eth_getBlockByNumber".to_string(),
    //         &(lagged_block.number.unwrap(), false),
    //         Some(Web3ProxyBlock::try_from(head_block.clone()).unwrap()),
    //         Some(Duration::from_millis(100)),
    //     )
    //     .await
    //     .unwrap();
    //     assert!(matches!(
    //         rpcs.wait_for_best_rpc(&r, &mut vec![], None).await,
    //         Ok(OpenRequestResult::Handle(_))
    //     ));

    //     // request on the head block should get a handle
    //     // TODO: make sure the handle is for the expected rpc
    //     let r = ValidatedRequest::new_internal(
    //         "eth_getBlockByNumber".to_string(),
    //         &(head_block.number.unwrap(), false),
    //         Some(Web3ProxyBlock::try_from(head_block.clone()).unwrap()),
    //         Some(Duration::from_millis(100)),
    //     )
    //     .await
    //     .unwrap();
    //     assert!(matches!(
    //         rpcs.wait_for_best_rpc(&r, &mut vec![], None,).await,
    //         Ok(OpenRequestResult::Handle(_))
    //     ));

    //     /*
    //     // TODO: bring this back. it is failing because there is no global APP and so things default to not needing caching. no cache checks means we don't know this is a future block
    //     // future block should not get a handle
    //     let future_block_num = head_block.as_ref().number.unwrap() + U64::from(10);
    //     let r = ValidatedRequest::new_internal(
    //         "eth_getBlockByNumber".to_string(),
    //         &(future_block_num, false),
    //         Some(Web3ProxyBlock::try_from(head_block.clone()).unwrap()),
    //         Some(Duration::from_millis(100)),
    //     )
    //     .await.unwrap();
    //     let future_rpc = rpcs.wait_for_best_rpc(&r, &mut vec![], None).await;

    //     info!(?future_rpc);

    //     // TODO: is this an ok or an error?
    //     assert!(matches!(future_rpc, Ok(OpenRequestResult::NotReady)));
    //     */
    // }

    // #[test_log::test(tokio::test)]
    // async fn test_server_selection_when_not_enough() {
    //     let now = chrono::Utc::now().timestamp().into();

    //     let head_block = Block {
    //         hash: Some(H256::random()),
    //         number: Some(1_000_000.into()),
    //         parent_hash: H256::random(),
    //         timestamp: now,
    //         ..Default::default()
    //     };

    //     let head_block: Web3ProxyBlock = Arc::new(head_block).try_into().unwrap();

    //     let lagged_rpc = Web3Rpc {
    //         name: "lagged".to_string(),
    //         soft_limit: 3_000,
    //         automatic_block_limit: false,
    //         backup: false,
    //         block_data_limit: 64.into(),
    //         tier: 1.into(),
    //         head_block_sender: None,
    //         ..Default::default()
    //     };

    //     assert!(!lagged_rpc.has_block_data(head_block.number()));

    //     let lagged_rpc = Arc::new(lagged_rpc);

    //     let (block_sender, _) = mpsc::unbounded_channel();
    //     let (watch_ranked_rpcs, _) = watch::channel(None);
    //     let (watch_consensus_head_sender, _watch_consensus_head_receiver) = watch::channel(None);

    //     let chain_id = 1;

    //     let mut by_name = HashMap::new();
    //     by_name.insert(lagged_rpc.name.clone(), lagged_rpc.clone());

    //     let rpcs = Web3Rpcs {
    //         block_sender,
    //         blocks_by_hash: CacheBuilder::new(100).build(),
    //         blocks_by_number: CacheBuilder::new(100).build(),
    //         by_name: RwLock::new(by_name),
    //         chain_id,
    //         max_head_block_age: Duration::from_secs(60),
    //         max_head_block_lag: 5.into(),
    //         min_sum_soft_limit: 100,
    //         min_synced_rpcs: 2,
    //         name: "test".into(),
    //         pending_txid_firehose_sender: None,
    //         watch_head_block: Some(watch_consensus_head_sender),
    //         watch_ranked_rpcs,
    //     };

    //     let mut connection_heads = ConsensusFinder::new(None, None);

    //     // min sum soft limit will require 2 servers
    //     let x = connection_heads
    //         .process_block_from_rpc(&rpcs, Some(head_block.clone()), lagged_rpc.clone())
    //         .await
    //         .unwrap();
    //     assert!(!x);

    //     assert_eq!(rpcs.num_synced_rpcs(), 0);

    //     // best_synced_backend_connection requires servers to be synced with the head block
    //     let r = ValidatedRequest::new_internal(
    //         "eth_getBlockByNumber".to_string(),
    //         &("latest", false),
    //         Some(head_block.clone()),
    //         Some(Duration::from_millis(100)),
    //     )
    //     .await
    //     .unwrap();
    //     let best_available_server = rpcs.wait_for_best_rpc(&r, &mut vec![], None).await.unwrap();

    //     debug!("best_available_server: {:#?}", best_available_server);

    //     assert!(matches!(best_available_server, OpenRequestResult::NotReady));
    // }

    // #[test_log::test(tokio::test)]
    // #[ignore = "refactor needed to make this work properly. it passes but only after waiting for long timeouts"]
    // async fn test_server_selection_by_archive() {
    //     let now = chrono::Utc::now().timestamp().into();

    //     let head_block = Block {
    //         hash: Some(H256::random()),
    //         number: Some(1_000_000.into()),
    //         parent_hash: H256::random(),
    //         timestamp: now,
    //         ..Default::default()
    //     };

    //     let head_block: Web3ProxyBlock = Arc::new(head_block).try_into().unwrap();

    //     let (tx_pruned, _) = watch::channel(Some(head_block.clone()));

    //     let pruned_rpc = Web3Rpc {
    //         name: "pruned".to_string(),
    //         soft_limit: 3_000,
    //         automatic_block_limit: false,
    //         backup: false,
    //         block_data_limit: 64.into(),
    //         tier: 1.into(),
    //         head_block_sender: Some(tx_pruned),
    //         ..Default::default()
    //     };

    //     let (tx_archive, _) = watch::channel(Some(head_block.clone()));

    //     let archive_rpc = Web3Rpc {
    //         name: "archive".to_string(),
    //         soft_limit: 1_000,
    //         automatic_block_limit: false,
    //         backup: false,
    //         block_data_limit: u64::MAX.into(),
    //         tier: 2.into(),
    //         head_block_sender: Some(tx_archive),
    //         ..Default::default()
    //     };

    //     assert!(pruned_rpc.has_block_data(head_block.number()));
    //     assert!(archive_rpc.has_block_data(head_block.number()));
    //     assert!(!pruned_rpc.has_block_data(1.into()));
    //     assert!(archive_rpc.has_block_data(1.into()));

    //     let pruned_rpc = Arc::new(pruned_rpc);
    //     let archive_rpc = Arc::new(archive_rpc);

    //     let (block_sender, _) = mpsc::unbounded_channel();
    //     let (watch_ranked_rpcs, _) = watch::channel(None);
    //     let (watch_consensus_head_sender, _watch_consensus_head_receiver) = watch::channel(None);

    //     let chain_id = 1;

    //     let mut by_name = HashMap::new();
    //     by_name.insert(pruned_rpc.name.clone(), pruned_rpc.clone());
    //     by_name.insert(archive_rpc.name.clone(), archive_rpc.clone());

    //     let rpcs = Web3Rpcs {
    //         block_sender,
    //         blocks_by_hash: CacheBuilder::new(100).build(),
    //         blocks_by_number: CacheBuilder::new(100).build(),
    //         by_name: RwLock::new(by_name),
    //         chain_id,
    //         max_head_block_age: Duration::from_secs(60),
    //         max_head_block_lag: 5.into(),
    //         min_sum_soft_limit: 4_000,
    //         min_synced_rpcs: 1,
    //         name: "test".into(),
    //         pending_txid_firehose_sender: None,
    //         watch_head_block: Some(watch_consensus_head_sender),
    //         watch_ranked_rpcs,
    //     };

    //     let mut connection_heads = ConsensusFinder::new(None, None);

    //     // min sum soft limit will require 2 servers
    //     let x = connection_heads
    //         .process_block_from_rpc(&rpcs, Some(head_block.clone()), pruned_rpc.clone())
    //         .await
    //         .unwrap();
    //     assert!(!x);

    //     assert_eq!(rpcs.num_synced_rpcs(), 0);

    //     let x = connection_heads
    //         .process_block_from_rpc(&rpcs, Some(head_block.clone()), archive_rpc.clone())
    //         .await
    //         .unwrap();
    //     assert!(x);

    //     assert_eq!(rpcs.num_synced_rpcs(), 2);

    //     // best_synced_backend_connection requires servers to be synced with the head block
    //     // TODO: test with and without passing the head_block.number?
    //     let r = ValidatedRequest::new_internal(
    //         "eth_getBlockByNumber".to_string(),
    //         &(head_block.number(), false),
    //         Some(head_block.clone()),
    //         Some(Duration::from_millis(100)),
    //     )
    //     .await
    //     .unwrap();
    //     let best_available_server = rpcs.wait_for_best_rpc(&r, &mut vec![], None).await.unwrap();

    //     debug!("best_available_server: {:#?}", best_available_server);

    //     assert!(matches!(
    //         best_available_server,
    //         OpenRequestResult::Handle(_)
    //     ));

    //     let r = ValidatedRequest::new_internal(
    //         "eth_getBlockByNumber".to_string(),
    //         &(head_block.number(), false),
    //         Some(head_block.clone()),
    //         Some(Duration::from_millis(100)),
    //     )
    //     .await
    //     .unwrap();
    //     let _best_available_server_from_none =
    //         rpcs.wait_for_best_rpc(&r, &mut vec![], None).await.unwrap();

    //     // assert_eq!(best_available_server, best_available_server_from_none);

    //     // TODO: actually test a future block. this ValidatedRequest doesn't require block #1
    //     let r = ValidatedRequest::new_internal(
    //         "eth_getBlockByNumber".to_string(),
    //         &(head_block.number(), false),
    //         Some(head_block.clone()),
    //         Some(Duration::from_millis(100)),
    //     )
    //     .await
    //     .unwrap();
    //     let best_archive_server = rpcs.wait_for_best_rpc(&r, &mut vec![], None).await;

    //     match best_archive_server {
    //         Ok(OpenRequestResult::Handle(x)) => {
    //             assert_eq!(x.clone_connection().name, "archive".to_string())
    //         }
    //         x => {
    //             panic!("unexpected result: {:?}", x);
    //         }
    //     }
    // }

    // #[test_log::test(tokio::test)]
    // #[ignore = "needs a rewrite that uses anvil or mocks the provider. i thought process_block_from_rpc was enough but i was wrong"]
    // async fn test_all_connections() {
    //     // TODO: use chrono, not SystemTime
    //     let now: U256 = SystemTime::now()
    //         .duration_since(UNIX_EPOCH)
    //         .unwrap()
    //         .as_secs()
    //         .into();

    //     let geth_data_limit = 64u64;

    //     let block_archive = Block {
    //         hash: Some(H256::random()),
    //         number: Some((1_000_000 - geth_data_limit * 2).into()),
    //         parent_hash: H256::random(),
    //         timestamp: now - geth_data_limit * 2,
    //         ..Default::default()
    //     };
    //     let block_1 = Block {
    //         hash: Some(H256::random()),
    //         number: Some(1_000_000.into()),
    //         parent_hash: H256::random(),
    //         timestamp: now - 1,
    //         ..Default::default()
    //     };
    //     let block_2 = Block {
    //         hash: Some(H256::random()),
    //         number: Some(1_000_001.into()),
    //         parent_hash: block_1.hash.unwrap(),
    //         timestamp: now,
    //         ..Default::default()
    //     };

    //     let block_archive: Web3ProxyBlock = Arc::new(block_archive).try_into().unwrap();
    //     let block_1: Web3ProxyBlock = Arc::new(block_1).try_into().unwrap();
    //     let block_2: Web3ProxyBlock = Arc::new(block_2).try_into().unwrap();

    //     let (tx_mock_geth, _) = watch::channel(Some(block_1.clone()));
    //     let (tx_mock_erigon_archive, _) = watch::channel(Some(block_2.clone()));

    //     let mock_geth = Web3Rpc {
    //         name: "mock_geth".to_string(),
    //         soft_limit: 1_000,
    //         automatic_block_limit: false,
    //         backup: false,
    //         block_data_limit: geth_data_limit.into(),
    //         head_block_sender: Some(tx_mock_geth),
    //         peak_latency: Some(new_peak_latency()),
    //         ..Default::default()
    //     };

    //     let mock_erigon_archive = Web3Rpc {
    //         name: "mock_erigon_archive".to_string(),
    //         soft_limit: 1_000,
    //         automatic_block_limit: false,
    //         backup: false,
    //         block_data_limit: u64::MAX.into(),
    //         head_block_sender: Some(tx_mock_erigon_archive),
    //         peak_latency: Some(new_peak_latency()),
    //         ..Default::default()
    //     };

    //     assert!(!mock_geth.has_block_data(block_archive.number()));
    //     assert!(mock_erigon_archive.has_block_data(block_archive.number()));
    //     assert!(mock_geth.has_block_data(block_1.number()));
    //     assert!(mock_erigon_archive.has_block_data(block_1.number()));
    //     assert!(!mock_geth.has_block_data(block_2.number()));
    //     assert!(mock_erigon_archive.has_block_data(block_2.number()));

    //     let mock_geth = Arc::new(mock_geth);
    //     let mock_erigon_archive = Arc::new(mock_erigon_archive);

    //     let (block_sender, _) = mpsc::unbounded_channel();
    //     let (watch_ranked_rpcs, _) = watch::channel(None);
    //     let (watch_consensus_head_sender, _watch_consensus_head_receiver) = watch::channel(None);

    //     let chain_id = 1;

    //     let mut by_name = HashMap::new();
    //     by_name.insert(mock_geth.name.clone(), mock_geth.clone());
    //     by_name.insert(
    //         mock_erigon_archive.name.clone(),
    //         mock_erigon_archive.clone(),
    //     );

    //     // TODO: make a Web3Rpcs::new
    //     let rpcs = Web3Rpcs {
    //         block_sender,
    //         blocks_by_hash: Cache::new(100),
    //         blocks_by_number: Cache::new(100),
    //         by_name: RwLock::new(by_name),
    //         chain_id,
    //         max_head_block_age: Duration::from_secs(60),
    //         max_head_block_lag: 5.into(),
    //         min_sum_soft_limit: 1_000,
    //         min_synced_rpcs: 1,
    //         name: "test".into(),
    //         pending_txid_firehose_sender: None,
    //         watch_head_block: Some(watch_consensus_head_sender),
    //         watch_ranked_rpcs,
    //     };

    //     let mut consensus_finder = ConsensusFinder::new(None, None);

    //     consensus_finder
    //         .process_block_from_rpc(
    //             &rpcs,
    //             Some(block_archive.clone()),
    //             mock_erigon_archive.clone(),
    //         )
    //         .await
    //         .unwrap();

    //     consensus_finder
    //         .process_block_from_rpc(&rpcs, Some(block_1.clone()), mock_erigon_archive.clone())
    //         .await
    //         .unwrap();

    //     consensus_finder
    //         .process_block_from_rpc(&rpcs, Some(block_1.clone()), mock_geth.clone())
    //         .await
    //         .unwrap();

    //     consensus_finder
    //         .process_block_from_rpc(&rpcs, Some(block_2.clone()), mock_erigon_archive.clone())
    //         .await
    //         .unwrap();

    //     assert_eq!(rpcs.num_synced_rpcs(), 1);

    //     // best_synced_backend_connection requires servers to be synced with the head block
    //     let r = ValidatedRequest::new_internal(
    //         "eth_getBlockByNumber".to_string(),
    //         &(block_2.number(), false),
    //         Some(block_2.clone()),
    //         Some(Duration::from_millis(100)),
    //     )
    //     .await;
    //     let head_connections = rpcs.all_connections(&r, None, None).await;

    //     debug!("head_connections: {:#?}", head_connections);

    //     assert_eq!(
    //         head_connections.unwrap().len(),
    //         1,
    //         "wrong number of connections"
    //     );

    //     // this should give us both servers
    //     let r = ValidatedRequest::new_internal(
    //         "eth_getBlockByNumber".to_string(),
    //         &(block_1.number(), false),
    //         Some(block_2.clone()),
    //         Some(Duration::from_millis(100)),
    //     )
    //     .await
    //     .unwrap();

    //     match &r.cache_mode {
    //         CacheMode::Standard {
    //             block,
    //             cache_errors,
    //         } => {
    //             assert_eq!(block, &BlockNumAndHash::from(&block_1));
    //             assert!(cache_errors);
    //         }
    //         x => {
    //             panic!("unexpected CacheMode: {:?}", x);
    //         }
    //     }

    //     let all_connections = rpcs.all_connections(&r, None, None).await;

    //     debug!("all_connections: {:#?}", all_connections);

    //     assert_eq!(
    //         all_connections.unwrap().len(),
    //         2,
    //         "wrong number of connections"
    //     );

    //     // this should give us only the archive server
    //     // TODO: i think this might have problems because block_1 - 100 isn't a real block and so queries for it will fail. then it falls back to caching with the head block
    //     // TODO: what if we check if its an archive block while doing cache_mode.
    //     let r = ValidatedRequest::new_internal(
    //         "eth_getBlockByNumber".to_string(),
    //         &(block_archive.number(), false),
    //         Some(block_2.clone()),
    //         Some(Duration::from_millis(100)),
    //     )
    //     .await
    //     .unwrap();

    //     match &r.cache_mode {
    //         CacheMode::Standard {
    //             block,
    //             cache_errors,
    //         } => {
    //             assert_eq!(block, &BlockNumAndHash::from(&block_archive));
    //             assert!(cache_errors);
    //         }
    //         x => {
    //             panic!("unexpected CacheMode: {:?}", x);
    //         }
    //     }

    //     let all_connections = rpcs.all_connections(&r, None, None).await;

    //     debug!("all_connections: {:#?}", all_connections);

    //     assert_eq!(
    //         all_connections.unwrap().len(),
    //         1,
    //         "wrong number of connections"
    //     );
    // }

    #[test]
    fn test_block_num_sort() {
        let test_vec = vec![
            Reverse(Some(3)),
            Reverse(Some(2)),
            Reverse(Some(1)),
            Reverse(None),
        ];

        let mut sorted_vec = test_vec.clone();
        sorted_vec.sort();

        assert_eq!(test_vec, sorted_vec);
    }

    #[test]
    fn test_bool_sort() {
        let test_vec = vec![false, true];

        let mut sorted_vec = test_vec.clone();
        sorted_vec.sort();

        assert_eq!(test_vec, sorted_vec);
    }
}
