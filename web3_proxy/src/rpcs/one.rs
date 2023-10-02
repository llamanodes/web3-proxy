//! Rate-limited communication with a web3 provider.
use super::blockchain::{ArcBlock, BlocksByHashCache, Web3ProxyBlock};
use super::provider::{connect_ws, EthersWsProvider};
use super::request::{OpenRequestHandle, OpenRequestResult};
use crate::app::{flatten_handle, Web3ProxyJoinHandle};
use crate::config::{BlockAndRpc, Web3RpcConfig};
use crate::errors::{Web3ProxyError, Web3ProxyErrorContext, Web3ProxyResult};
use crate::frontend::authorization::Web3Request;
use crate::jsonrpc::{self, JsonRpcParams, JsonRpcResultData};
use crate::rpcs::request::RequestErrorHandler;
use anyhow::{anyhow, Context};
use arc_swap::ArcSwapOption;
use ethers::prelude::{Address, Bytes, Middleware, Transaction, TxHash, U256, U64};
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use latency::{EwmaLatency, PeakEwmaLatency, RollingQuantileLatency};
use migration::sea_orm::DatabaseConnection;
use nanorand::Rng;
use redis_rate_limiter::{RedisPool, RedisRateLimitResult, RedisRateLimiter};
use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;
use serde_json::json;
use std::cmp::Reverse;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{self, AtomicU32, AtomicU64, AtomicUsize};
use std::{cmp::Ordering, sync::Arc};
use tokio::sync::{mpsc, watch, RwLock as AsyncRwLock};
use tokio::time::{interval, sleep, sleep_until, Duration, Instant, MissedTickBehavior};
use tracing::{debug, error, info, trace, warn, Level};
use url::Url;

/// An active connection to a Web3 RPC server like geth or erigon.
#[derive(Default)]
pub struct Web3Rpc {
    pub name: String,
    pub chain_id: u64,
    pub block_interval: Duration,
    pub display_name: Option<String>,
    pub db_conn: Option<DatabaseConnection>,
    pub subscribe_txs: bool,
    /// most all requests prefer use the http_provider
    pub(super) http_client: Option<reqwest::Client>,
    pub(super) http_url: Option<Url>,
    /// the websocket url is only used for subscriptions
    pub(super) ws_url: Option<Url>,
    /// the websocket provider is only used for subscriptions
    pub(super) ws_provider: ArcSwapOption<EthersWsProvider>,
    /// keep track of hard limits
    /// hard_limit_until is only inside an Option so that the "Default" derive works. it will always be set.
    pub(super) hard_limit_until: Option<watch::Sender<Instant>>,
    /// rate limits are stored in a central redis so that multiple proxies can share their rate limits
    /// We do not use the deferred rate limiter because going over limits would cause errors
    pub(super) hard_limit: Option<RedisRateLimiter>,
    /// used for ensuring enough requests are available before advancing the head block
    pub(super) soft_limit: u32,
    /// use web3 queries to find the block data limit for archive/pruned nodes
    pub(super) automatic_block_limit: bool,
    /// only use this rpc if everything else is lagging too far. this allows us to ignore fast but very low limit rpcs
    pub backup: bool,
    /// TODO: have an enum for this so that "no limit" prints pretty?
    pub(super) block_data_limit: AtomicU64,
    /// head_block is only inside an Option so that the "Default" derive works. it will always be set.
    pub(super) head_block_sender: Option<watch::Sender<Option<Web3ProxyBlock>>>,
    /// Track head block latency.
    pub(super) head_delay: AsyncRwLock<EwmaLatency>,
    /// Track peak request latency
    /// peak_latency is only inside an Option so that the "Default" derive works. it will always be set.
    pub(super) peak_latency: Option<PeakEwmaLatency>,
    /// Automatically set priority
    pub(super) tier: AtomicU32,
    /// Track total internal requests served
    pub(super) internal_requests: AtomicUsize,
    /// Track total external requests served
    pub(super) external_requests: AtomicUsize,
    /// If the head block is too old, it is ignored.
    pub(super) max_head_block_age: Duration,
    /// Track time used by external requests served
    /// request_ms_histogram is only inside an Option so that the "Default" derive works. it will always be set.
    pub(super) median_latency: Option<RollingQuantileLatency>,
    /// Track in-flight requests
    pub(super) active_requests: AtomicUsize,
    /// disconnect_watch is only inside an Option so that the "Default" derive works. it will always be set.
    pub(super) disconnect_watch: Option<watch::Sender<bool>>,
    /// created_at is only inside an Option so that the "Default" derive works. it will always be set.
    pub(super) created_at: Option<Instant>,
}

impl Web3Rpc {
    /// Connect to a web3 rpc
    // TODO: have this take a builder (which will have channels attached). or maybe just take the config and give the config public fields
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn(
        config: Web3RpcConfig,
        name: String,
        chain_id: u64,
        // optional because this is only used for http providers. websocket-only providers don't use it
        http_client: Option<reqwest::Client>,
        redis_pool: Option<RedisPool>,
        server_id: i64,
        block_interval: Duration,
        block_map: BlocksByHashCache,
        block_and_rpc_sender: Option<mpsc::UnboundedSender<BlockAndRpc>>,
        pending_txid_firehose_sender: Option<mpsc::Sender<TxHash>>,
        max_head_block_age: Duration,
    ) -> anyhow::Result<(Arc<Web3Rpc>, Web3ProxyJoinHandle<()>)> {
        let created_at = Instant::now();

        let hard_limit = match (config.hard_limit, redis_pool) {
            (None, None) => None,
            (Some(hard_limit), Some(redis_pool)) => {
                let label = if config.hard_limit_per_endpoint {
                    format!("{}:{}:{}", chain_id, "endpoint", name)
                } else {
                    format!("{}:{}:{}", chain_id, server_id, name)
                };

                // TODO: in process rate limiter instead? or maybe deferred? or is this good enough?
                let rrl = RedisRateLimiter::new(
                    "web3_proxy",
                    &label,
                    hard_limit,
                    config.hard_limit_period as f32,
                    redis_pool,
                );

                Some(rrl)
            }
            (None, Some(_)) => None,
            (Some(_hard_limit), None) => {
                return Err(anyhow::anyhow!(
                    "no redis client pool! needed for hard limit"
                ))
            }
        };

        let backup = config.backup;

        let block_data_limit: AtomicU64 = config.block_data_limit.into();
        let automatic_block_limit = (block_data_limit.load(atomic::Ordering::Relaxed) == 0)
            && block_and_rpc_sender.is_some();

        // have a sender for tracking hard limit anywhere. we use this in case we
        // and track on servers that have a configured hard limit
        let (hard_limit_until, _) = watch::channel(Instant::now());

        if config.ws_url.is_none() && config.http_url.is_none() {
            return Err(anyhow!(
                "either ws_url or http_url are required. it is best to set both. they must both point to the same server!"
            ));
        }

        let (head_block, _) = watch::channel(None);

        // Spawn the task for calculting average peak latency
        // TODO Should these defaults be in config
        let peak_latency = PeakEwmaLatency::spawn(
            // Decay over 15s
            Duration::from_secs(15),
            // Peak requests so far around 5k, we will use an order of magnitude
            // more to be safe. Should only use about 50mb RAM
            50_000,
            // Start latency at 1 second
            Duration::from_secs(1),
        );

        let median_request_latency = RollingQuantileLatency::spawn_median(1_000).await;

        let (http_url, http_client) = if let Some(http_url) = config.http_url {
            let http_url = http_url.parse::<Url>()?;
            // TODO: double-check not missing anything from connect_http()
            let http_client = http_client.unwrap_or_default();
            (Some(http_url), Some(http_client))
        } else {
            (None, None)
        };

        let ws_url = if let Some(ws_url) = config.ws_url {
            let ws_url = ws_url.parse::<Url>()?;

            Some(ws_url)
        } else {
            None
        };

        let (disconnect_watch, _) = watch::channel(false);

        let new_rpc = Self {
            automatic_block_limit,
            backup,
            block_data_limit,
            block_interval,
            created_at: Some(created_at),
            display_name: config.display_name,
            hard_limit,
            hard_limit_until: Some(hard_limit_until),
            head_block_sender: Some(head_block),
            http_url,
            http_client,
            max_head_block_age,
            name,
            peak_latency: Some(peak_latency),
            median_latency: Some(median_request_latency),
            soft_limit: config.soft_limit,
            subscribe_txs: config.subscribe_txs,
            ws_url,
            disconnect_watch: Some(disconnect_watch),
            ..Default::default()
        };

        let new_connection = Arc::new(new_rpc);

        // subscribe to new blocks and new transactions
        // subscribing starts the connection (with retries)
        let handle = {
            let new_connection = new_connection.clone();
            tokio::spawn(async move {
                new_connection
                    .subscribe_with_reconnect(
                        block_map,
                        block_and_rpc_sender,
                        pending_txid_firehose_sender,
                        chain_id,
                    )
                    .await
            })
        };

        Ok((new_connection, handle))
    }

    /// sort by...
    /// - backups last
    /// - tier (ascending)
    /// - block number (descending)
    /// TODO: tests on this!
    /// TODO: should tier or block number take priority?
    /// TODO: should this return a struct that implements sorting traits?
    /// TODO: move this to consensus.rs
    fn sort_on(&self, max_block: Option<U64>) -> (bool, Reverse<U64>, u32) {
        let mut head_block = self
            .head_block_sender
            .as_ref()
            .and_then(|x| x.borrow().as_ref().map(|x| *x.number()))
            .unwrap_or_default();

        if let Some(max_block) = max_block {
            head_block = head_block.min(max_block);
        }

        let tier = self.tier.load(atomic::Ordering::Relaxed);

        let backup = self.backup;

        (!backup, Reverse(head_block), tier)
    }

    /// TODO: move this to consensus.rs
    pub fn sort_for_load_balancing_on(
        &self,
        max_block: Option<U64>,
    ) -> ((bool, Reverse<U64>, u32), Duration) {
        let sort_on = self.sort_on(max_block);

        let weighted_peak_latency = self.weighted_peak_latency();

        let x = (sort_on, weighted_peak_latency);

        trace!("sort_for_load_balancing {}: {:?}", self, x);

        x
    }

    /// like sort_for_load_balancing, but shuffles tiers randomly instead of sorting by weighted_peak_latency
    /// TODO: move this to consensus.rs
    pub fn shuffle_for_load_balancing_on(
        &self,
        max_block: Option<U64>,
    ) -> ((bool, Reverse<U64>, u32), u8) {
        let sort_on = self.sort_on(max_block);

        let mut rng = nanorand::tls_rng();

        let r = rng.generate::<u8>();

        (sort_on, r)
    }

    pub fn weighted_peak_latency(&self) -> Duration {
        let peak_latency = if let Some(peak_latency) = self.peak_latency.as_ref() {
            peak_latency.latency()
        } else {
            Duration::from_secs(1)
        };

        // TODO: what scaling?
        // TODO: figure out how many requests add what level of latency
        let request_scaling = 0.01;
        // TODO: what ordering?
        let active_requests =
            self.active_requests.load(atomic::Ordering::Relaxed) as f32 * request_scaling + 1.0;

        peak_latency.mul_f32(active_requests)
    }

    // TODO: would be great if rpcs exposed this. see https://github.com/ledgerwatch/erigon/issues/6391
    async fn check_block_data_limit(self: &Arc<Self>) -> anyhow::Result<Option<u64>> {
        if !self.automatic_block_limit {
            // TODO: is this a good thing to return?
            return Ok(None);
        }

        // TODO: check eth_syncing. if it is not false, return Ok(None)

        let mut limit = None;

        // TODO: binary search between 90k and max?
        // TODO: start at 0 or 1?
        for block_data_limit in [0, 32, 64, 128, 256, 512, 1024, 90_000, u64::MAX] {
            let head_block_num = self
                .internal_request::<_, U256>(
                    "eth_blockNumber",
                    &[(); 0],
                    // error here are expected, so keep the level low
                    Some(Level::DEBUG.into()),
                    Some(Duration::from_secs(5)),
                )
                .await
                .context("head_block_num error during check_block_data_limit")?;

            let maybe_archive_block = head_block_num.saturating_sub((block_data_limit).into());

            trace!(
                "checking maybe_archive_block on {}: {}",
                self,
                maybe_archive_block
            );

            // TODO: wait for the handle BEFORE we check the current block number. it might be delayed too!
            // TODO: what should the request be?
            let archive_result: Result<Bytes, _> = self
                .internal_request(
                    "eth_getCode",
                    &json!((
                        "0xdead00000000000000000000000000000000beef",
                        maybe_archive_block,
                    )),
                    // error here are expected, so keep the level low
                    Some(Level::TRACE.into()),
                    Some(Duration::from_secs(5)),
                )
                .await;

            trace!(
                "archive_result on {} for {} ({}): {:?}",
                self,
                block_data_limit,
                maybe_archive_block,
                archive_result
            );

            if archive_result.is_err() {
                break;
            }

            limit = Some(block_data_limit);
        }

        if let Some(limit) = limit {
            if limit == 0 {
                warn!("{} is unable to serve requests", self);
            }

            self.block_data_limit
                .store(limit, atomic::Ordering::Relaxed);
        }

        if limit == Some(u64::MAX) {
            info!("block data limit on {}: archive", self);
        } else {
            info!("block data limit on {}: {:?}", self, limit);
        }

        Ok(limit)
    }

    /// TODO: this might be too simple. different nodes can prune differently. its possible we will have a block range
    pub fn block_data_limit(&self) -> U64 {
        self.block_data_limit.load(atomic::Ordering::Acquire).into()
    }

    /// TODO: get rid of this now that consensus rpcs does it
    pub fn has_block_data(&self, needed_block_num: &U64) -> bool {
        let head_block_num = match self.head_block_sender.as_ref().unwrap().borrow().as_ref() {
            None => return false,
            Some(x) => *x.number(),
        };

        // this rpc doesn't have that block yet. still syncing
        if needed_block_num > &head_block_num {
            trace!(
                "{} has head {} but needs {}",
                self,
                head_block_num,
                needed_block_num,
            );
            return false;
        }

        // if this is a pruning node, we might not actually have the block
        let block_data_limit: U64 = self.block_data_limit();

        let oldest_block_num = head_block_num.saturating_sub(block_data_limit);

        if needed_block_num < &oldest_block_num {
            trace!(
                "{} needs {} but the oldest available is {}",
                self,
                needed_block_num,
                oldest_block_num
            );
            return false;
        }

        true
    }

    /// query the web3 provider to confirm it is on the expected chain with the expected data available
    /// TODO: this currently checks only the http if both http and ws are set. it should check both and make sure they match
    async fn check_provider(self: &Arc<Self>, chain_id: u64) -> Web3ProxyResult<()> {
        // check the server's chain_id here
        // TODO: some public rpcs (on bsc and fantom) do not return an id and so this ends up being an error
        // TODO: what should the timeout be? should there be a request timeout?
        // trace!("waiting on chain id for {}", self);
        let found_chain_id: U64 = self
            .internal_request(
                "eth_chainId",
                &[(); 0],
                Some(Level::TRACE.into()),
                Some(Duration::from_secs(5)),
            )
            .await?;

        trace!("found_chain_id: {:#?}", found_chain_id);

        if chain_id != found_chain_id.as_u64() {
            return Err(anyhow::anyhow!(
                "incorrect chain id! Config has {}, but RPC has {}",
                chain_id,
                found_chain_id
            )
            .context(format!("failed @ {}", self))
            .into());
        }

        // TODO: only do this for balanced_rpcs. this errors on 4337 rpcs
        self.check_block_data_limit()
            .await
            .context(format!("unable to check_block_data_limit of {}", self))?;

        info!("successfully connected to {}", self);

        Ok(())
    }

    pub(crate) async fn send_head_block_result(
        self: &Arc<Self>,
        new_head_block: Web3ProxyResult<Option<ArcBlock>>,
        block_and_rpc_sender: &mpsc::UnboundedSender<BlockAndRpc>,
        block_map: &BlocksByHashCache,
    ) -> Web3ProxyResult<()> {
        let head_block_sender = self.head_block_sender.as_ref().unwrap();

        let new_head_block = match new_head_block {
            Ok(x) => {
                let x = x.and_then(Web3ProxyBlock::try_new);

                match x {
                    None => {
                        if head_block_sender.borrow().is_none() {
                            // we previously sent a None. return early
                            return Ok(());
                        }

                        let age = self.created_at.unwrap().elapsed().as_millis();

                        trace!("clearing head block on {} ({}ms old)!", self, age);

                        // send an empty block to take this server out of rotation
                        head_block_sender.send_replace(None);

                        // TODO: clear self.block_data_limit?

                        None
                    }
                    Some(new_head_block) => {
                        let new_hash = *new_head_block.hash();

                        // if we already have this block saved, set new_head_block to that arc. otherwise store this copy
                        let new_head_block = block_map
                            .get_with_by_ref(&new_hash, async move { new_head_block })
                            .await;

                        // we are synced! yey!
                        head_block_sender.send_replace(Some(new_head_block.clone()));

                        if self.block_data_limit() == U64::zero() {
                            if let Err(err) = self.check_block_data_limit().await {
                                warn!(
                                    "failed checking block limit after {} finished syncing. {:?}",
                                    self, err
                                );
                            }
                        }

                        Some(new_head_block)
                    }
                }
            }
            Err(err) => {
                warn!(?err, "unable to get block from {}", self);

                // send an empty block to take this server out of rotation
                head_block_sender.send_replace(None);

                // TODO: clear self.block_data_limit?

                None
            }
        };

        // tell web3rpcs about this rpc having this block
        block_and_rpc_sender
            .send((new_head_block, self.clone()))
            .context("block_and_rpc_sender failed sending")?;

        Ok(())
    }

    fn should_disconnect(&self) -> bool {
        *self.disconnect_watch.as_ref().unwrap().borrow()
    }

    async fn healthcheck(
        self: &Arc<Self>,
        error_handler: Option<RequestErrorHandler>,
    ) -> Web3ProxyResult<()> {
        let head_block = self.head_block_sender.as_ref().unwrap().borrow().clone();

        if let Some(head_block) = head_block {
            // TODO: if head block is very old and not expected to be syncing, emit warning
            if head_block.age() > self.max_head_block_age {
                return Err(anyhow::anyhow!("head_block is too old!").into());
            }

            let block_number = *head_block.number();

            let to = if let Some(txid) = head_block.transactions().last().cloned() {
                let tx = self
                    .internal_request::<_, Option<Transaction>>(
                        "eth_getTransactionByHash",
                        &(txid,),
                        error_handler,
                        Some(Duration::from_secs(5)),
                    )
                    .await?
                    .context("no transaction")?;

                // TODO: what default? something real?
                tx.to.unwrap_or_else(|| {
                    "0xdead00000000000000000000000000000000beef"
                        .parse::<Address>()
                        .expect("deafbeef")
                })
            } else {
                "0xdead00000000000000000000000000000000beef"
                    .parse::<Address>()
                    .expect("deafbeef")
            };

            let _code = self
                .internal_request::<_, Option<Bytes>>(
                    "eth_getCode",
                    &(to, block_number),
                    error_handler,
                    Some(Duration::from_secs(5)),
                )
                .await?;
        } else {
            // TODO: if head block is none for too long, give an error
        }

        Ok(())
    }

    /// TODO: this needs to be a subscribe_with_reconnect that does a retry with jitter and exponential backoff
    async fn subscribe_with_reconnect(
        self: Arc<Self>,
        block_map: BlocksByHashCache,
        block_and_rpc_sender: Option<mpsc::UnboundedSender<BlockAndRpc>>,
        pending_txid_firehose_sender: Option<mpsc::Sender<TxHash>>,
        chain_id: u64,
    ) -> Web3ProxyResult<()> {
        loop {
            if let Err(err) = self
                .clone()
                .subscribe(
                    block_map.clone(),
                    block_and_rpc_sender.clone(),
                    pending_txid_firehose_sender.clone(),
                    chain_id,
                )
                .await
            {
                if self.should_disconnect() {
                    break;
                }

                warn!(?err, "subscribe err on {}", self);
            } else if self.should_disconnect() {
                break;
            }

            if self.backup {
                debug!("reconnecting to {} in 30 seconds", self);
            } else {
                info!("reconnecting to {} in 30 seconds", self);
            }

            // TODO: exponential backoff with jitter
            sleep(Duration::from_secs(30)).await;
        }

        Ok(())
    }

    /// subscribe to blocks and transactions
    /// This should only exit when the program is exiting.
    /// TODO: should more of these args be on self? chain_id for sure
    async fn subscribe(
        self: Arc<Self>,
        block_map: BlocksByHashCache,
        block_and_rpc_sender: Option<mpsc::UnboundedSender<BlockAndRpc>>,
        pending_txid_firehose_sender: Option<mpsc::Sender<TxHash>>,
        chain_id: u64,
    ) -> Web3ProxyResult<()> {
        let error_handler = if self.backup {
            Some(RequestErrorHandler::DebugLevel)
        } else {
            // TODO: info level?
            Some(RequestErrorHandler::InfoLevel)
        };

        if self.should_disconnect() {
            return Ok(());
        }

        if let Some(url) = self.ws_url.clone() {
            trace!("starting websocket provider on {}", self);

            let x = connect_ws(url, usize::MAX).await?;

            let x = Arc::new(x);

            self.ws_provider.store(Some(x));
        }

        if self.should_disconnect() {
            return Ok(());
        }

        trace!("starting subscriptions on {}", self);

        self.check_provider(chain_id)
            .await
            .web3_context("failed check_provider")?;

        let mut futures = FuturesUnordered::new();

        // TODO: use this channel instead of self.disconnect_watch
        let (subscribe_stop_tx, subscribe_stop_rx) = watch::channel(false);

        // subscribe to the disconnect watch. the app uses this when shutting down or when configs change
        if let Some(disconnect_watch_tx) = self.disconnect_watch.as_ref() {
            let rpc = self.clone();
            let mut disconnect_watch_rx = disconnect_watch_tx.subscribe();

            let f = async move {
                loop {
                    if *disconnect_watch_rx.borrow_and_update() {
                        break;
                    }

                    disconnect_watch_rx.changed().await?;
                }
                trace!("disconnect triggered on {}", rpc);
                Ok(())
            };

            futures.push(flatten_handle(tokio::spawn(f)));
        } else {
            unimplemented!("there should always be a disconnect watch!");
        }

        // health check that runs if there haven't been any recent requests
        if block_and_rpc_sender.is_some() {
            // TODO: move this into a proper function
            let rpc = self.clone();

            // TODO: how often? different depending on the chain?
            // TODO: reset this timeout when a new block is seen? we need to keep median_request_latency updated though
            let health_sleep_seconds = 5;

            // health check loop
            let f = async move {
                // TODO: benchmark this and lock contention
                let mut old_total_requests = 0;
                let mut new_total_requests;

                // errors here should not cause the loop to exit!
                while !(*subscribe_stop_rx.borrow()) {
                    new_total_requests = rpc.internal_requests.load(atomic::Ordering::Relaxed)
                        + rpc.external_requests.load(atomic::Ordering::Relaxed);

                    if new_total_requests - old_total_requests < 5 {
                        // TODO: if this fails too many times, reset the connection
                        // TODO: move this into a function and the chaining should be easier
                        if let Err(err) = rpc.healthcheck(error_handler).await {
                            // TODO: different level depending on the error handler
                            // TODO: if rate limit error, set "retry_at"
                            if rpc.backup {
                                warn!(?err, "health check on {} failed", rpc);
                            } else {
                                error!(?err, "health check on {} failed", rpc);
                            }
                        }
                    }

                    // TODO: should we count the requests done inside this health check
                    old_total_requests = new_total_requests;

                    sleep(Duration::from_secs(health_sleep_seconds)).await;
                }

                trace!("healthcheck loop on {} exited", rpc);

                Ok(())
            };

            futures.push(flatten_handle(tokio::spawn(f)));
        }

        // subscribe to new heads
        if let Some(block_and_rpc_sender) = block_and_rpc_sender.clone() {
            let clone = self.clone();
            let subscribe_stop_rx = subscribe_stop_tx.subscribe();
            let block_map = block_map.clone();

            let f = async move {
                clone
                    .subscribe_new_heads(block_and_rpc_sender.clone(), block_map, subscribe_stop_rx)
                    .await
            };

            futures.push(flatten_handle(tokio::spawn(f)));
        }

        // subscribe to new transactions
        if let Some(pending_txid_firehose) = pending_txid_firehose_sender.clone() {
            let clone = self.clone();
            let subscribe_stop_rx = subscribe_stop_tx.subscribe();

            let f = async move {
                clone
                    .subscribe_new_transactions(pending_txid_firehose, subscribe_stop_rx)
                    .await
            };

            futures.push(flatten_handle(tokio::spawn(f)));
        }

        // exit if any of the futures exit
        // TODO: have an enum for which one exited?
        let first_exit = futures.next().await;

        debug!(?first_exit, "subscriptions on {} exited", self);

        // clear the head block
        if let Some(block_and_rpc_sender) = block_and_rpc_sender {
            self.send_head_block_result(Ok(None), &block_and_rpc_sender, &block_map)
                .await?
        };

        subscribe_stop_tx.send_replace(true);

        // TODO: wait for all of the futures to exit?

        // TODO: tell ethers to disconnect?
        self.ws_provider.store(None);

        Ok(())
    }

    async fn subscribe_new_transactions(
        self: &Arc<Self>,
        pending_txid_firehose: mpsc::Sender<TxHash>,
        mut subscribe_stop_rx: watch::Receiver<bool>,
    ) -> Web3ProxyResult<()> {
        trace!("subscribing to new transactions on {}", self);

        // rpcs opt-into subscribing to transactions. its a lot of bandwidth
        if !self.subscribe_txs {
            loop {
                if *subscribe_stop_rx.borrow_and_update() {
                    trace!("stopping ws block subscription on {}", self);
                    return Ok(());
                }
                subscribe_stop_rx.changed().await?;
            }
        }

        if let Some(ws_provider) = self.ws_provider.load().as_ref() {
            // todo: move subscribe_blocks onto the request handle
            let authorization = Default::default();

            let error_handler = Some(Level::ERROR.into());

            let active_request_handle = self
                .wait_for_request_handle(&authorization, None, error_handler)
                .await;

            let mut pending_txs_sub = ws_provider.subscribe_pending_txs().await?;

            drop(active_request_handle);

            while let Some(x) = pending_txs_sub.next().await {
                if *subscribe_stop_rx.borrow_and_update() {
                    // TODO: this is checking way too often. have this on a timer instead
                    trace!("stopping ws block subscription on {}", self);
                    break;
                }

                // this should always work
                if let Err(err) = pending_txid_firehose.try_send(x) {
                    error!(
                        ?err,
                        "pending_txid_firehose failed sending. it must be full"
                    );
                }
            }
        } else {
            // TODO: what should we do here?
            loop {
                if *subscribe_stop_rx.borrow_and_update() {
                    trace!("stopping ws block subscription on {}", self);
                    return Ok(());
                }
                subscribe_stop_rx.changed().await?;
            }
        }

        Ok(())
    }

    /// Subscribe to new blocks.
    async fn subscribe_new_heads(
        self: &Arc<Self>,
        block_sender: mpsc::UnboundedSender<BlockAndRpc>,
        block_map: BlocksByHashCache,
        mut subscribe_stop_rx: watch::Receiver<bool>,
    ) -> Web3ProxyResult<()> {
        trace!("subscribing to new heads on {}", self);

        // TODO: different handler depending on backup or not
        let error_handler = if self.backup {
            Some(Level::DEBUG.into())
        } else {
            Some(Level::ERROR.into())
        };

        if let Some(ws_provider) = self.ws_provider.load().as_ref() {
            // todo: move subscribe_blocks onto the request handle
            let authorization = Default::default();

            let active_request_handle = self
                .wait_for_request_handle(&authorization, None, error_handler)
                .await;
            let mut blocks = ws_provider.subscribe_blocks().await?;
            drop(active_request_handle);

            // query the block once since the subscription doesn't send the current block
            // there is a very small race condition here where the stream could send us a new block right now
            // but all seeing the same block twice won't break anything
            // TODO: how does this get wrapped in an arc? does ethers handle that?
            // TODO: send this request to the ws_provider instead of the http_provider
            let latest_block: Result<Option<ArcBlock>, _> = self
                .internal_request(
                    "eth_getBlockByNumber",
                    &("latest", false),
                    error_handler,
                    Some(Duration::from_secs(5)),
                )
                .await;

            self.send_head_block_result(latest_block, &block_sender, &block_map)
                .await?;

            while let Some(block) = blocks.next().await {
                if *subscribe_stop_rx.borrow() {
                    trace!("stopping ws block subscription on {}", self);
                    break;
                }

                let block = Arc::new(block);

                self.send_head_block_result(Ok(Some(block)), &block_sender, &block_map)
                    .await?;
            }
        } else if self.http_client.is_some() {
            // there is a "watch_blocks" function, but a lot of public nodes (including llamanodes) do not support the necessary rpc endpoints
            // TODO: is 1/2 the block time okay?
            let mut i = interval(self.block_interval / 2);
            i.set_missed_tick_behavior(MissedTickBehavior::Delay);

            loop {
                if *subscribe_stop_rx.borrow_and_update() {
                    trace!(%self, "stopping http block subscription");
                    break;
                }

                let block_result = self
                    .internal_request::<_, Option<ArcBlock>>(
                        "eth_getBlockByNumber",
                        &("latest", false),
                        error_handler,
                        Some(Duration::from_secs(5)),
                    )
                    .await;

                self.send_head_block_result(block_result, &block_sender, &block_map)
                    .await?;

                i.tick().await;
            }
        } else {
            return Err(anyhow!("no ws or http provider!").into());
        }

        // clear the head block. this might not be needed, but it won't hurt
        self.send_head_block_result(Ok(None), &block_sender, &block_map)
            .await?;

        if *subscribe_stop_rx.borrow() {
            trace!(%self, "new heads subscription exited");
            Ok(())
        } else {
            Err(anyhow!("new_heads subscription exited. reconnect needed").into())
        }
    }

    pub async fn wait_for_request_handle(
        self: &Arc<Self>,
        web3_request: &Arc<Web3Request>,
        max_wait: Option<Duration>,
        error_handler: Option<RequestErrorHandler>,
    ) -> Web3ProxyResult<OpenRequestHandle> {
        // TODO: what should the default be?
        // TODO: split max_wait_connect (which might wait if a rate limit is pending) and max_wait_request
        let max_wait_until = max_wait.map(|x| Instant::now() + x);

        loop {
            match self.try_request_handle(web3_request, error_handler).await {
                Ok(OpenRequestResult::Handle(handle)) => return Ok(handle),
                Ok(OpenRequestResult::RetryAt(retry_at)) => {
                    // TODO: emit a stat?
                    let wait = retry_at.duration_since(Instant::now());

                    trace!(
                        "waiting {} millis for request handle on {}",
                        wait.as_millis(),
                        self
                    );

                    if let Some(max_wait_until) = max_wait_until {
                        if retry_at > max_wait_until {
                            // break now since we will wait past our maximum wait time
                            return Err(Web3ProxyError::Timeout(None));
                        }
                    }

                    sleep_until(retry_at).await;
                }
                Ok(OpenRequestResult::NotReady) => {
                    // TODO: when can this happen? log? emit a stat?
                    trace!("{} has no handle ready", self);

                    if let Some(max_wait_until) = max_wait_until {
                        if Instant::now() > max_wait_until {
                            return Err(Web3ProxyError::NoHandleReady);
                        }
                    }

                    // TODO: sleep how long? maybe just error?
                    // TODO: instead of an arbitrary sleep, subscribe to the head block on this?
                    sleep(Duration::from_millis(10)).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    pub async fn try_request_handle(
        self: &Arc<Self>,
        web3_request: &Arc<Web3Request>,
        error_handler: Option<RequestErrorHandler>,
    ) -> Web3ProxyResult<OpenRequestResult> {
        // TODO: if websocket is reconnecting, return an error?

        // check cached rate limits
        if let Some(hard_limit_until) = self.hard_limit_until.as_ref() {
            let hard_limit_ready = *hard_limit_until.borrow();
            let now = Instant::now();
            if now < hard_limit_ready {
                return Ok(OpenRequestResult::RetryAt(hard_limit_ready));
            }
        }

        // check shared rate limits
        if let Some(ratelimiter) = self.hard_limit.as_ref() {
            // TODO: how should we know if we should set expire or not?
            match ratelimiter
                .throttle()
                .await
                .context(format!("attempting to throttle {}", self))?
            {
                RedisRateLimitResult::Allowed(_) => {
                    // trace!("rate limit succeeded")
                }
                RedisRateLimitResult::RetryAt(retry_at, _) => {
                    // rate limit gave us a wait time
                    // if not a backup server, warn. backups hit rate limits often
                    if !self.backup {
                        let when = retry_at.duration_since(Instant::now());
                        warn!(
                            retry_ms=%when.as_millis(),
                            "Exhausted rate limit on {}",
                            self,
                        );
                    }

                    if let Some(hard_limit_until) = self.hard_limit_until.as_ref() {
                        hard_limit_until.send_replace(retry_at);
                    }

                    return Ok(OpenRequestResult::RetryAt(retry_at));
                }
                RedisRateLimitResult::RetryNever => {
                    warn!("how did retry never on {} happen?", self);
                    return Ok(OpenRequestResult::NotReady);
                }
            }
        };

        let handle =
            OpenRequestHandle::new(web3_request.clone(), self.clone(), error_handler).await;

        Ok(handle.into())
    }

    pub async fn internal_request<P: JsonRpcParams, R: JsonRpcResultData>(
        self: &Arc<Self>,
        method: &str,
        params: &P,
        error_handler: Option<RequestErrorHandler>,
        max_wait: Option<Duration>,
    ) -> Web3ProxyResult<R> {
        let web3_request = Web3Request::new_internal(method.into(), params, None).await;

        self.authorized_request(&web3_request, error_handler, max_wait)
            .await
    }

    pub async fn authorized_request<R: JsonRpcResultData>(
        self: &Arc<Self>,
        web3_request: &Arc<Web3Request>,
        error_handler: Option<RequestErrorHandler>,
        max_wait: Option<Duration>,
    ) -> Web3ProxyResult<R> {
        let handle = self
            .wait_for_request_handle(web3_request, max_wait, error_handler)
            .await?;

        let response = handle.request().await?;
        let parsed = response.parsed().await?;
        match parsed.payload {
            jsonrpc::Payload::Success { result } => Ok(result),
            jsonrpc::Payload::Error { error } => Err(error.into()),
        }
    }
}

impl Hash for Web3Rpc {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // do not include automatic block limit because it can change
        // do not include tier because it can change
        self.backup.hash(state);
        self.created_at.hash(state);
        self.display_name.hash(state);
        self.name.hash(state);

        // TODO: url does NOT include the authorization data. i think created_at should protect us if auth changes without anything else
        self.http_url.hash(state);
        // TODO: figure out how to get the url for the ws provider
        // self.ws_provider.map(|x| x.url()).hash(state);

        // TODO: don't include soft_limit if we change them to be dynamic
        self.soft_limit.hash(state);
    }
}

impl Eq for Web3Rpc {}

impl Ord for Web3Rpc {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.name.cmp(&other.name)
    }
}

impl PartialOrd for Web3Rpc {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Web3Rpc {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Serialize for Web3Rpc {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // 14 if we bring head_delay back
        let mut state = serializer.serialize_struct("Web3Rpc", 13)?;

        // the url is excluded because it likely includes private information. just show the name that we use in keys
        state.serialize_field("name", &self.name)?;
        // a longer name for display to users
        state.serialize_field("display_name", &self.display_name)?;

        state.serialize_field("backup", &self.backup)?;

        match self.block_data_limit.load(atomic::Ordering::Acquire) {
            u64::MAX => {
                state.serialize_field("block_data_limit", &None::<()>)?;
            }
            block_data_limit => {
                state.serialize_field("block_data_limit", &block_data_limit)?;
            }
        }

        state.serialize_field("tier", &self.tier)?;

        state.serialize_field("soft_limit", &self.soft_limit)?;

        // TODO: maybe this is too much data. serialize less?
        {
            let head_block = self.head_block_sender.as_ref().unwrap();
            let head_block = head_block.borrow();
            let head_block = head_block.as_ref();
            state.serialize_field("head_block", &head_block)?;
        }

        state.serialize_field(
            "external_requests",
            &self.external_requests.load(atomic::Ordering::Relaxed),
        )?;

        state.serialize_field(
            "internal_requests",
            &self.internal_requests.load(atomic::Ordering::Relaxed),
        )?;

        state.serialize_field(
            "active_requests",
            &self.active_requests.load(atomic::Ordering::Relaxed),
        )?;

        // {
        //     let head_delay_ms = self.head_delay.read().await.latency().as_secs_f32() * 1000.0;
        //     state.serialize_field("head_delay_ms", &(head_delay_ms))?;
        // }

        {
            let median_latency_ms = self
                .median_latency
                .as_ref()
                .unwrap()
                .latency()
                .as_secs_f32()
                * 1000.0;
            state.serialize_field("median_latency_ms", &(median_latency_ms))?;
        }

        {
            let peak_latency_ms =
                self.peak_latency.as_ref().unwrap().latency().as_secs_f32() * 1000.0;
            state.serialize_field("peak_latency_ms", &peak_latency_ms)?;
        }
        {
            let weighted_latency_ms = self.weighted_peak_latency().as_secs_f32() * 1000.0;
            state.serialize_field("weighted_latency_ms", &weighted_latency_ms)?;
        }

        state.end()
    }
}

impl fmt::Debug for Web3Rpc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut f = f.debug_struct("Web3Rpc");

        f.field("name", &self.name);

        let block_data_limit = self.block_data_limit.load(atomic::Ordering::Acquire);
        if block_data_limit == u64::MAX {
            f.field("blocks", &"all");
        } else {
            f.field("blocks", &block_data_limit);
        }

        f.field("backup", &self.backup);

        f.field("tier", &self.tier.load(atomic::Ordering::Relaxed));

        f.field("weighted_ms", &self.weighted_peak_latency().as_millis());

        if let Some(head_block_watch) = self.head_block_sender.as_ref() {
            if let Some(head_block) = head_block_watch.borrow().as_ref() {
                f.field("head_num", head_block.number());
                f.field("head_hash", head_block.hash());
            } else {
                f.field("head_num", &None::<()>);
                f.field("head_hash", &None::<()>);
            }
        }

        f.finish_non_exhaustive()
    }
}

impl fmt::Display for Web3Rpc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", &self.name)
    }
}

mod tests {
    #![allow(unused_imports)]
    use super::*;
    use ethers::types::{Block, H256, U256};

    #[test]
    fn test_archive_node_has_block_data() {
        let now = chrono::Utc::now().timestamp().into();

        let random_block = Block {
            hash: Some(H256::random()),
            number: Some(1_000_000.into()),
            timestamp: now,
            ..Default::default()
        };

        let random_block = Arc::new(random_block);

        let head_block = Web3ProxyBlock::try_new(random_block).unwrap();
        let block_data_limit = u64::MAX;

        let (tx, _) = watch::channel(Some(head_block.clone()));

        let x = Web3Rpc {
            name: "name".to_string(),
            soft_limit: 1_000,
            automatic_block_limit: false,
            backup: false,
            block_data_limit: block_data_limit.into(),
            head_block_sender: Some(tx),
            ..Default::default()
        };

        assert!(x.has_block_data(&0.into()));
        assert!(x.has_block_data(&1.into()));
        assert!(x.has_block_data(head_block.number()));
        assert!(!x.has_block_data(&(head_block.number() + 1)));
        assert!(!x.has_block_data(&(head_block.number() + 1000)));
    }

    #[test]
    fn test_pruned_node_has_block_data() {
        let now = chrono::Utc::now().timestamp().into();

        let head_block: Web3ProxyBlock = Arc::new(Block {
            hash: Some(H256::random()),
            number: Some(1_000_000.into()),
            timestamp: now,
            ..Default::default()
        })
        .try_into()
        .unwrap();

        let block_data_limit = 64;

        let (tx, _rx) = watch::channel(Some(head_block.clone()));

        let x = Web3Rpc {
            name: "name".to_string(),
            soft_limit: 1_000,
            automatic_block_limit: false,
            backup: false,
            block_data_limit: block_data_limit.into(),
            head_block_sender: Some(tx),
            ..Default::default()
        };

        assert!(!x.has_block_data(&0.into()));
        assert!(!x.has_block_data(&1.into()));
        assert!(!x.has_block_data(&(head_block.number() - block_data_limit - 1)));
        assert!(x.has_block_data(&(head_block.number() - block_data_limit)));
        assert!(x.has_block_data(head_block.number()));
        assert!(!x.has_block_data(&(head_block.number() + 1)));
        assert!(!x.has_block_data(&(head_block.number() + 1000)));
    }

    /*
    // TODO: think about how to bring the concept of a "lagged" node back
    #[test]
    fn test_lagged_node_not_has_block_data() {
        let now = chrono::Utc::now().timestamp().into();

        // head block is an hour old
        let head_block = Block {
            hash: Some(H256::random()),
            number: Some(1_000_000.into()),
            timestamp: now - 3600,
            ..Default::default()
        };

        let head_block = Arc::new(head_block);

        let head_block = Web3ProxyBlock::new(head_block);
        let block_data_limit = u64::MAX;

        let metrics = OpenRequestHandleMetrics::default();

        let x = Web3Rpc {
            name: "name".to_string(),
            db_conn: None,
            display_name: None,
            url: "ws://example.com".to_string(),
            http_client: None,
            active_requests: 0.into(),
            frontend_requests: 0.into(),
            internal_requests: 0.into(),
            provider_state: AsyncRwLock::new(ProviderState::None),
            hard_limit: None,
            soft_limit: 1_000,
            automatic_block_limit: false,
            backup: false,
            block_data_limit: block_data_limit.into(),
            tier: 0,
            head_block: AsyncRwLock::new(Some(head_block.clone())),
        };

        assert!(!x.has_block_data(&0.into()));
        assert!(!x.has_block_data(&1.into()));
        assert!(!x.has_block_data(&head_block.number()));
        assert!(!x.has_block_data(&(head_block.number() + 1)));
        assert!(!x.has_block_data(&(head_block.number() + 1000)));
    }
    */
}
