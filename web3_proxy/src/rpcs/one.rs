///! Rate-limited communication with a web3 provider.
use super::blockchain::{ArcBlock, BlocksByHashCache, Web3ProxyBlock};
use super::provider::Web3Provider;
use super::request::{OpenRequestHandle, OpenRequestResult};
use crate::app::{flatten_handle, AnyhowJoinHandle};
use crate::config::{BlockAndRpc, Web3RpcConfig};
use crate::frontend::authorization::Authorization;
use crate::frontend::errors::{Web3ProxyError, Web3ProxyResult};
use crate::rpcs::request::RequestErrorHandler;
use anyhow::{anyhow, Context};
use ethers::prelude::{Bytes, Middleware, ProviderError, TxHash, H256, U64};
use ethers::types::{Address, Transaction, U256};
use futures::future::try_join_all;
use futures::StreamExt;
use latency::{EwmaLatency, PeakEwmaLatency};
use log::{debug, error, info, trace, warn, Level};
use migration::sea_orm::DatabaseConnection;
use ordered_float::OrderedFloat;
use parking_lot::RwLock;
use redis_rate_limiter::{RedisPool, RedisRateLimitResult, RedisRateLimiter};
use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;
use serde_json::json;
use std::cmp::min;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{self, AtomicBool, AtomicU64, AtomicUsize};
use std::{cmp::Ordering, sync::Arc};
use thread_fast_rng::rand::Rng;
use thread_fast_rng::thread_fast_rng;
use tokio::sync::{broadcast, oneshot, watch, RwLock as AsyncRwLock};
use tokio::time::{sleep, sleep_until, timeout, Duration, Instant};

/// An active connection to a Web3 RPC server like geth or erigon.
#[derive(Default)]
pub struct Web3Rpc {
    pub name: String,
    pub display_name: Option<String>,
    pub db_conn: Option<DatabaseConnection>,
    pub(super) ws_url: Option<String>,
    pub(super) http_url: Option<String>,
    /// Some connections use an http_client. we keep a clone for reconnecting
    pub(super) http_client: Option<reqwest::Client>,
    /// provider is in a RwLock so that we can replace it if re-connecting
    /// it is an async lock because we hold it open across awaits
    /// this provider is only used for new heads subscriptions
    /// TODO: watch channel instead of a lock
    /// TODO: is this only used for new heads subscriptions? if so, rename
    pub(super) provider: AsyncRwLock<Option<Arc<Web3Provider>>>,
    /// keep track of hard limits. Optional because we skip this code for our own servers.
    pub(super) hard_limit_until: Option<watch::Sender<Instant>>,
    /// rate limits are stored in a central redis so that multiple proxies can share their rate limits
    /// We do not use the deferred rate limiter because going over limits would cause errors
    pub(super) hard_limit: Option<RedisRateLimiter>,
    /// used for load balancing to the least loaded server
    pub(super) soft_limit: u32,
    /// use web3 queries to find the block data limit for archive/pruned nodes
    pub(super) automatic_block_limit: bool,
    /// only use this rpc if everything else is lagging too far. this allows us to ignore fast but very low limit rpcs
    pub backup: bool,
    /// TODO: have an enum for this so that "no limit" prints pretty?
    pub(super) block_data_limit: AtomicU64,
    /// Lower tiers are higher priority when sending requests
    pub(super) tier: u64,
    /// TODO: change this to a watch channel so that http providers can subscribe and take action on change.
    pub(super) head_block: RwLock<Option<Web3ProxyBlock>>,
    /// Track head block latency
    pub(super) head_latency: RwLock<EwmaLatency>,
    /// Track peak request latency
    ///
    /// This is only inside an Option so that the "Default" derive works. it will always be set.
    pub(super) peak_latency: Option<PeakEwmaLatency>,
    /// Track total requests served
    /// TODO: maybe move this to graphana
    pub(super) total_requests: AtomicUsize,
    pub(super) active_requests: AtomicUsize,
    pub(super) reconnect: AtomicBool,
    /// this is only inside an Option so that the "Default" derive works. it will always be set.
    pub(super) disconnect_watch: Option<watch::Sender<bool>>,
    pub(super) created_at: Option<Instant>,
}

impl Web3Rpc {
    /// Connect to a web3 rpc
    // TODO: have this take a builder (which will have channels attached). or maybe just take the config and give the config public fields
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn(
        mut config: Web3RpcConfig,
        name: String,
        chain_id: u64,
        db_conn: Option<DatabaseConnection>,
        // optional because this is only used for http providers. websocket providers don't use it
        http_client: Option<reqwest::Client>,
        // TODO: rename to http_new_head_interval_sender?
        http_interval_sender: Option<Arc<broadcast::Sender<()>>>,
        redis_pool: Option<RedisPool>,
        // TODO: think more about soft limit. watching ewma of requests is probably better. but what should the random sort be on? maybe group on tier is enough
        // soft_limit: u32,
        block_map: BlocksByHashCache,
        block_sender: Option<flume::Sender<BlockAndRpc>>,
        tx_id_sender: Option<flume::Sender<(TxHash, Arc<Self>)>>,
        reconnect: bool,
    ) -> anyhow::Result<(Arc<Web3Rpc>, AnyhowJoinHandle<()>)> {
        let created_at = Instant::now();

        let hard_limit = match (config.hard_limit, redis_pool) {
            (None, None) => None,
            (Some(hard_limit), Some(redis_pool)) => {
                // TODO: in process rate limiter instead? or is deffered good enough?
                let rrl = RedisRateLimiter::new(
                    "web3_proxy",
                    &format!("{}:{}", chain_id, name),
                    hard_limit,
                    60.0,
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

        let tx_id_sender = if config.subscribe_txs {
            tx_id_sender
        } else {
            None
        };

        let backup = config.backup;

        let block_data_limit: AtomicU64 = config.block_data_limit.unwrap_or_default().into();
        let automatic_block_limit =
            (block_data_limit.load(atomic::Ordering::Acquire) == 0) && block_sender.is_some();

        // track hard limit until on backup servers (which might surprise us with rate limit changes)
        // and track on servers that have a configured hard limit
        let hard_limit_until = if backup || hard_limit.is_some() {
            let (sender, _) = watch::channel(Instant::now());

            Some(sender)
        } else {
            None
        };

        if config.ws_url.is_none() && config.http_url.is_none() {
            if let Some(url) = config.url {
                if url.starts_with("ws") {
                    config.ws_url = Some(url);
                } else if url.starts_with("http") {
                    config.http_url = Some(url);
                } else {
                    return Err(anyhow!("only ws or http urls are supported"));
                }
            } else {
                return Err(anyhow!(
                    "either ws_url or http_url are required. it is best to set both"
                ));
            }
        }

        let (disconnect_sender, disconnect_receiver) = watch::channel(false);
        let reconnect = reconnect.into();

        // Spawn the task for calculting average peak latency
        // TODO: is one second good for the decay here?

        const NANOS_PER_MILLI: f64 = 1_000_000.0;
        // TODO good defaults? should they be config?
        let peak_latency =
            PeakEwmaLatency::spawn(1_000.0 * NANOS_PER_MILLI, 4, Duration::from_secs(1));

        let new_connection = Self {
            name,
            db_conn: db_conn.clone(),
            display_name: config.display_name,
            http_client,
            ws_url: config.ws_url,
            http_url: config.http_url,
            hard_limit,
            hard_limit_until,
            soft_limit: config.soft_limit,
            automatic_block_limit,
            backup,
            block_data_limit,
            reconnect,
            tier: config.tier,
            disconnect_watch: Some(disconnect_sender),
            created_at: Some(created_at),
            head_block: RwLock::new(Default::default()),
            peak_latency: Some(peak_latency),
            ..Default::default()
        };

        let new_connection = Arc::new(new_connection);

        // subscribe to new blocks and new transactions
        // subscribing starts the connection (with retries)
        // TODO: make transaction subscription optional (just pass None for tx_id_sender)
        let handle = {
            let new_connection = new_connection.clone();
            let authorization = Arc::new(Authorization::internal(db_conn)?);
            tokio::spawn(async move {
                new_connection
                    .subscribe(
                        &authorization,
                        block_map,
                        block_sender,
                        chain_id,
                        disconnect_receiver,
                        http_interval_sender,
                        tx_id_sender,
                    )
                    .await
            })
        };

        Ok((new_connection, handle))
    }

    pub fn peak_ewma(&self) -> OrderedFloat<f64> {
        // TODO: use request instead of head latency? that was killing perf though
        let head_ewma = self.head_latency.read().value();

        // TODO: what ordering?
        let active_requests = self.active_requests.load(atomic::Ordering::Relaxed) as f64;

        // TODO: i'm not sure head * active is exactly right. but we'll see
        // TODO: i don't think this actually counts as peak. investigate with atomics.rs and peak_ewma.rs
        OrderedFloat(head_ewma * active_requests)
    }

    // TODO: would be great if rpcs exposed this. see https://github.com/ledgerwatch/erigon/issues/6391
    async fn check_block_data_limit(
        self: &Arc<Self>,
        authorization: &Arc<Authorization>,
        unlocked_provider: Option<Arc<Web3Provider>>,
    ) -> anyhow::Result<Option<u64>> {
        if !self.automatic_block_limit {
            // TODO: is this a good thing to return?
            return Ok(None);
        }

        // TODO: check eth_syncing. if it is not false, return Ok(None)

        let mut limit = None;

        // TODO: binary search between 90k and max?
        // TODO: start at 0 or 1?
        for block_data_limit in [0, 32, 64, 128, 256, 512, 1024, 90_000, u64::MAX] {
            let handle = self
                .wait_for_request_handle(authorization, None, unlocked_provider.clone())
                .await?;

            let head_block_num_future = handle.request::<Option<()>, U256>(
                "eth_blockNumber",
                &None,
                // error here are expected, so keep the level low
                Level::Debug.into(),
                unlocked_provider.clone(),
            );

            let head_block_num = timeout(Duration::from_secs(5), head_block_num_future)
                .await
                .context("timeout fetching eth_blockNumber")?
                .context("provider error")?;

            let maybe_archive_block = head_block_num.saturating_sub((block_data_limit).into());

            trace!(
                "checking maybe_archive_block on {}: {}",
                self,
                maybe_archive_block
            );

            // TODO: wait for the handle BEFORE we check the current block number. it might be delayed too!
            // TODO: what should the request be?
            let handle = self
                .wait_for_request_handle(authorization, None, unlocked_provider.clone())
                .await?;

            let archive_result: Result<Bytes, _> = handle
                .request(
                    "eth_getCode",
                    &json!((
                        "0xdead00000000000000000000000000000000beef",
                        maybe_archive_block,
                    )),
                    // error here are expected, so keep the level low
                    Level::Trace.into(),
                    unlocked_provider.clone(),
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
                .store(limit, atomic::Ordering::Release);
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

    pub fn has_block_data(&self, needed_block_num: &U64) -> bool {
        let head_block_num = match self.head_block.read().as_ref() {
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

    /// reconnect to the provider. errors are retried forever with exponential backoff with jitter.
    /// We use the "Decorrelated" jitter from <https://aws.amazon.com/blogs/architecture/exponential-backoff-and-jitter/>
    /// TODO: maybe it would be better to use "Full Jitter". The "Full Jitter" approach uses less work, but slightly more time.
    pub async fn retrying_connect(
        self: &Arc<Self>,
        block_sender: Option<&flume::Sender<BlockAndRpc>>,
        chain_id: u64,
        db_conn: Option<&DatabaseConnection>,
        delay_start: bool,
    ) -> anyhow::Result<()> {
        // there are several crates that have retry helpers, but they all seem more complex than necessary
        // TODO: move this backoff logic into a helper function so we can use it when doing database locking
        let base_ms = 500;
        let cap_ms = 30_000;
        let range_multiplier = 3;

        // sleep once before the initial retry attempt
        // TODO: now that we use this method for our initial connection, do we still want this sleep?
        let mut sleep_ms = if delay_start {
            let first_sleep_ms = min(
                cap_ms,
                thread_fast_rng().gen_range(base_ms..(base_ms * range_multiplier)),
            );
            let reconnect_in = Duration::from_millis(first_sleep_ms);

            info!("Reconnect to {} in {}ms", self, reconnect_in.as_millis());

            sleep(reconnect_in).await;

            first_sleep_ms
        } else {
            base_ms
        };

        // retry until we succeed
        while let Err(err) = self.connect(block_sender, chain_id, db_conn).await {
            // thread_rng is crytographically secure. we don't need that here. use thread_fast_rng instead
            // TODO: min of 1 second? sleep longer if rate limited?
            sleep_ms = min(
                cap_ms,
                thread_fast_rng().gen_range(base_ms..(sleep_ms * range_multiplier)),
            );

            let retry_in = Duration::from_millis(sleep_ms);

            let error_level = if self.backup {
                log::Level::Debug
            } else {
                log::Level::Info
            };

            log::log!(
                error_level,
                "Failed (re)connect to {}! Retry in {}ms. err={:?}",
                self,
                retry_in.as_millis(),
                err,
            );

            sleep(retry_in).await;
        }

        Ok(())
    }

    /// connect to the web3 provider
    async fn connect(
        self: &Arc<Self>,
        block_sender: Option<&flume::Sender<BlockAndRpc>>,
        chain_id: u64,
        db_conn: Option<&DatabaseConnection>,
    ) -> anyhow::Result<()> {
        if let Ok(mut unlocked_provider) = self.provider.try_write() {
            #[cfg(test)]
            if let Some(Web3Provider::Mock) = unlocked_provider.as_deref() {
                return Ok(());
            }

            *unlocked_provider = if let Some(ws_url) = self.ws_url.as_ref() {
                // set up ws client
                match &*unlocked_provider {
                    None => {
                        info!("connecting to {}", self);
                    }
                    Some(_) => {
                        debug!("reconnecting to {}", self);

                        // tell the block subscriber that this rpc doesn't have any blocks
                        if let Some(block_sender) = block_sender {
                            block_sender
                                .send_async((None, self.clone()))
                                .await
                                .context("block_sender during connect")?;
                        }

                        // reset sync status
                        let mut head_block = self.head_block.write();
                        *head_block = None;

                        // disconnect the current provider
                        // TODO: what until the block_sender's receiver finishes updating this item?
                        *unlocked_provider = None;
                    }
                }

                let p = Web3Provider::from_str(ws_url.as_str(), None)
                    .await
                    .context(format!("failed connecting to {}", ws_url))?;

                assert!(p.ws().is_some());

                Some(Arc::new(p))
            } else {
                // http client
                if let Some(url) = &self.http_url {
                    let p = Web3Provider::from_str(url, self.http_client.clone())
                        .await
                        .context(format!("failed connecting to {}", url))?;

                    assert!(p.http().is_some());

                    Some(Arc::new(p))
                } else {
                    None
                }
            };

            let authorization = Arc::new(Authorization::internal(db_conn.cloned())?);

            // check the server's chain_id here
            // TODO: some public rpcs (on bsc and fantom) do not return an id and so this ends up being an error
            // TODO: what should the timeout be? should there be a request timeout?
            // trace!("waiting on chain id for {}", self);
            let found_chain_id: Result<U64, _> = self
                .wait_for_request_handle(&authorization, None, unlocked_provider.clone())
                .await
                .context(format!("waiting for request handle on {}", self))?
                .request(
                    "eth_chainId",
                    &json!(Option::None::<()>),
                    Level::Trace.into(),
                    unlocked_provider.clone(),
                )
                .await;
            // trace!("found_chain_id: {:?}", found_chain_id);

            match found_chain_id {
                Ok(found_chain_id) => {
                    // TODO: there has to be a cleaner way to do this
                    if chain_id != found_chain_id.as_u64() {
                        return Err(anyhow::anyhow!(
                            "incorrect chain id! Config has {}, but RPC has {}",
                            chain_id,
                            found_chain_id
                        )
                        .context(format!("failed @ {}", self)));
                    }
                }
                Err(e) => {
                    return Err(anyhow::Error::from(e)
                        .context(format!("unable to parse eth_chainId from {}", self)));
                }
            }

            self.check_block_data_limit(&authorization, unlocked_provider.clone())
                .await
                .context(format!("unable to check_block_data_limit of {}", self))?;

            drop(unlocked_provider);

            info!("successfully connected to {}", self);
        } else if self.provider.read().await.is_none() {
            return Err(anyhow!("failed waiting for client {}", self));
        };

        Ok(())
    }

    pub async fn disconnect(&self) -> anyhow::Result<()> {
        let age = self.created_at.unwrap().elapsed().as_secs();

        info!("disconnecting {} ({}s old)", self, age);

        self.reconnect.store(false, atomic::Ordering::Release);

        if let Err(err) = self.disconnect_watch.as_ref().unwrap().send(true) {
            warn!("failed sending disconnect watch: {:?}", err);
        };

        trace!("disconnecting (locking) {} ({}s old)", self, age);

        let mut provider = self.provider.write().await;

        trace!("disconnecting (clearing provider) {} ({}s old)", self, age);

        *provider = None;

        Ok(())
    }

    async fn send_head_block_result(
        self: &Arc<Self>,
        new_head_block: Result<Option<ArcBlock>, ProviderError>,
        block_sender: &flume::Sender<BlockAndRpc>,
        block_map: BlocksByHashCache,
    ) -> anyhow::Result<()> {
        let new_head_block = match new_head_block {
            Ok(None) => {
                {
                    let mut head_block = self.head_block.write();

                    if head_block.is_none() {
                        // we previously sent a None. return early
                        return Ok(());
                    }

                    let age = self.created_at.unwrap().elapsed().as_millis();

                    debug!("clearing head block on {} ({}ms old)!", self, age);

                    *head_block = None;
                }

                None
            }
            Ok(Some(new_head_block)) => {
                let new_head_block = Web3ProxyBlock::try_new(new_head_block)
                    .expect("blocks from newHeads subscriptions should also convert");

                let new_hash = *new_head_block.hash();

                // if we already have this block saved, set new_head_block to that arc. otherwise store this copy
                let new_head_block = block_map
                    .get_with(new_hash, async move { new_head_block })
                    .await;

                // save the block so we don't send the same one multiple times
                // also save so that archive checks can know how far back to query
                {
                    let mut head_block = self.head_block.write();

                    let _ = head_block.insert(new_head_block.clone());
                }

                if self.block_data_limit() == U64::zero() {
                    let authorization = Arc::new(Authorization::internal(self.db_conn.clone())?);
                    if let Err(err) = self.check_block_data_limit(&authorization, None).await {
                        warn!(
                            "failed checking block limit after {} finished syncing. {:?}",
                            self, err
                        );
                    }
                }

                Some(new_head_block)
            }
            Err(err) => {
                warn!("unable to get block from {}. err={:?}", self, err);

                {
                    let mut head_block = self.head_block.write();

                    *head_block = None;
                }

                None
            }
        };

        // send an empty block to take this server out of rotation
        block_sender
            .send_async((new_head_block, self.clone()))
            .await
            .context("block_sender")?;

        Ok(())
    }

    fn should_disconnect(&self) -> bool {
        *self.disconnect_watch.as_ref().unwrap().borrow()
    }

    /// subscribe to blocks and transactions with automatic reconnects
    /// This should only exit when the program is exiting.
    /// TODO: should more of these args be on self?
    #[allow(clippy::too_many_arguments)]
    async fn subscribe(
        self: Arc<Self>,
        authorization: &Arc<Authorization>,
        block_map: BlocksByHashCache,
        block_sender: Option<flume::Sender<BlockAndRpc>>,
        chain_id: u64,
        disconnect_receiver: watch::Receiver<bool>,
        http_interval_sender: Option<Arc<broadcast::Sender<()>>>,
        tx_id_sender: Option<flume::Sender<(TxHash, Arc<Self>)>>,
    ) -> anyhow::Result<()> {
        let error_handler = if self.backup {
            RequestErrorHandler::DebugLevel
        } else {
            RequestErrorHandler::ErrorLevel
        };

        let mut delay_start = false;

        // this does loop. just only when reconnect is enabled
        #[allow(clippy::never_loop)]
        loop {
            trace!("subscription loop started on {}", self);

            let mut futures = vec![];

            let http_interval_receiver = http_interval_sender.as_ref().map(|x| x.subscribe());

            {
                // TODO: move this into a proper function
                let authorization = authorization.clone();
                let block_sender = block_sender.clone();
                let rpc = self.clone();
                let (ready_tx, ready_rx) = oneshot::channel();
                let f = async move {
                    // initial sleep to allow for the initial connection
                    rpc.retrying_connect(
                        block_sender.as_ref(),
                        chain_id,
                        authorization.db_conn.as_ref(),
                        delay_start,
                    )
                    .await?;

                    // provider is ready
                    ready_tx.send(()).unwrap();

                    // TODO: how often? different depending on the chain?
                    // TODO: reset this timeout when a new block is seen? we need to keep request_latency updated though
                    let health_sleep_seconds = 10;

                    // TODO: benchmark this and lock contention
                    let mut old_total_requests = 0;
                    let mut new_total_requests;

                    // health check loop
                    loop {
                        // TODO: do we need this to be abortable?
                        if rpc.should_disconnect() {
                            break;
                        }

                        sleep(Duration::from_secs(health_sleep_seconds)).await;

                        trace!("health check on {}", rpc);

                        // TODO: what if we just happened to have this check line up with another restart?
                        // TODO: think more about this
                        if let Some(client) = rpc.provider.read().await.clone() {
                            // health check as a way of keeping this rpc's request_ewma accurate
                            // TODO: do something different if this is a backup server?

                            new_total_requests = rpc.total_requests.load(atomic::Ordering::Relaxed);

                            // TODO: how many requests should we require in order to skip a health check?
                            if new_total_requests - old_total_requests < 10 {
                                // TODO: if this fails too many times, reset the connection
                                // TODO: move this into a function and the chaining should be easier
                                let head_block = rpc.head_block.read().clone();

                                if let Some((block_number, txid)) = head_block.and_then(|x| {
                                    let block = x.block;

                                    let block_number = block.number?;
                                    let txid = block.transactions.last().cloned()?;

                                    Some((block_number, txid))
                                }) {
                                    let to = rpc
                                        .wait_for_query::<_, Option<Transaction>>(
                                            "eth_getTransactionByHash",
                                            &(txid,),
                                            error_handler,
                                            authorization.clone(),
                                            Some(client.clone()),
                                        )
                                        .await
                                        .and_then(|tx| {
                                            let tx = tx.context("no transaction found")?;

                                            // TODO: what default? something real?
                                            let to = tx.to.unwrap_or_else(|| {
                                                "0xdead00000000000000000000000000000000beef"
                                                    .parse::<Address>()
                                                    .expect("deafbeef")
                                            });

                                            Ok(to)
                                        });

                                    let code = match to {
                                        Err(err) => {
                                            if rpc.backup {
                                                debug!(
                                                    "{} failed health check query! {:#?}",
                                                    rpc, err
                                                );
                                            } else {
                                                warn!(
                                                    "{} failed health check query! {:#?}",
                                                    rpc, err
                                                );
                                            }
                                            continue;
                                        }
                                        Ok(to) => {
                                            rpc.wait_for_query::<_, Option<Bytes>>(
                                                "eth_getCode",
                                                &(to, block_number),
                                                error_handler,
                                                authorization.clone(),
                                                Some(client),
                                            )
                                            .await
                                        }
                                    };

                                    if let Err(err) = code {
                                        if rpc.backup {
                                            debug!("{} failed health check query! {:#?}", rpc, err);
                                        } else {
                                            warn!("{} failed health check query! {:#?}", rpc, err);
                                        }
                                        continue;
                                    }
                                }
                            }

                            old_total_requests = new_total_requests;
                        }
                    }
                    debug!("health checks for {} exited", rpc);

                    Ok(())
                };

                futures.push(flatten_handle(tokio::spawn(f)));

                // wait on the initial connection
                ready_rx.await?;
            }

            if let Some(block_sender) = &block_sender {
                // TODO: do we need this to be abortable?
                let f = self.clone().subscribe_new_heads(
                    authorization.clone(),
                    http_interval_receiver,
                    block_sender.clone(),
                    block_map.clone(),
                );

                futures.push(flatten_handle(tokio::spawn(f)));
            }

            if let Some(tx_id_sender) = &tx_id_sender {
                // TODO: do we need this to be abortable?
                let f = self
                    .clone()
                    .subscribe_pending_transactions(authorization.clone(), tx_id_sender.clone());

                futures.push(flatten_handle(tokio::spawn(f)));
            }

            match try_join_all(futures).await {
                Ok(_) => {
                    // future exited without error
                    // TODO: think about this more. we never set it to false. this can't be right
                    break;
                }
                Err(err) => {
                    let disconnect_sender = self.disconnect_watch.as_ref().unwrap();

                    if self.reconnect.load(atomic::Ordering::Acquire) {
                        warn!("{} connection ended. reconnecting. err={:?}", self, err);

                        // TODO: i'm not sure if this is necessary, but telling everything to disconnect seems like a better idea than relying on timeouts and dropped futures.
                        disconnect_sender.send_replace(true);
                        disconnect_sender.send_replace(false);

                        // we call retrying_connect here with initial_delay=true. above, initial_delay=false
                        delay_start = true;

                        continue;
                    }

                    // reconnect is not enabled.
                    if *disconnect_receiver.borrow() {
                        info!("{} is disconnecting", self);
                        break;
                    } else {
                        error!("{} subscription exited. err={:?}", self, err);

                        disconnect_sender.send_replace(true);

                        break;
                    }
                }
            }
        }

        info!("all subscriptions on {} completed", self);

        Ok(())
    }

    /// Subscribe to new blocks.
    async fn subscribe_new_heads(
        self: Arc<Self>,
        authorization: Arc<Authorization>,
        http_interval_receiver: Option<broadcast::Receiver<()>>,
        block_sender: flume::Sender<BlockAndRpc>,
        block_map: BlocksByHashCache,
    ) -> anyhow::Result<()> {
        trace!("watching new heads on {}", self);

        let provider = self.wait_for_provider().await;

        match provider.as_ref() {
            Web3Provider::Http(_client) => {
                // there is a "watch_blocks" function, but a lot of public nodes do not support the necessary rpc endpoints
                // TODO: try watch_blocks and fall back to this?

                let mut http_interval_receiver = http_interval_receiver.unwrap();

                let mut last_hash = H256::zero();

                while !self.should_disconnect() {
                    // TODO: what should the max_wait be?
                    // we do not pass unlocked_provider because we want to get a new one each call. otherwise we might re-use an old one
                    match self
                        .wait_for_request_handle(&authorization, None, None)
                        .await
                    {
                        Ok(active_request_handle) => {
                            let block: Result<Option<ArcBlock>, _> = active_request_handle
                                .request(
                                    "eth_getBlockByNumber",
                                    &json!(("latest", false)),
                                    Level::Warn.into(),
                                    None,
                                )
                                .await;

                            match block {
                                Ok(None) => {
                                    warn!("no head block on {}", self);

                                    self.send_head_block_result(
                                        Ok(None),
                                        &block_sender,
                                        block_map.clone(),
                                    )
                                    .await?;
                                }
                                Ok(Some(block)) => {
                                    // don't send repeat blocks
                                    let new_hash =
                                        block.hash.expect("blocks here should always have hashes");

                                    if new_hash != last_hash {
                                        // new hash!
                                        last_hash = new_hash;

                                        self.send_head_block_result(
                                            Ok(Some(block)),
                                            &block_sender,
                                            block_map.clone(),
                                        )
                                        .await?;
                                    }
                                }
                                Err(err) => {
                                    // we did not get a block back. something is up with the server. take it out of rotation
                                    self.send_head_block_result(
                                        Err(err),
                                        &block_sender,
                                        block_map.clone(),
                                    )
                                    .await?;
                                }
                            }
                        }
                        Err(err) => {
                            warn!("Internal error on latest block from {}. {:?}", self, err);

                            self.send_head_block_result(Ok(None), &block_sender, block_map.clone())
                                .await?;

                            // TODO: what should we do? sleep? extra time?
                        }
                    }

                    // wait for the next interval
                    // TODO: if error or rate limit, increase interval?
                    while let Err(err) = http_interval_receiver.recv().await {
                        match err {
                            broadcast::error::RecvError::Closed => {
                                // channel is closed! that's not good. bubble the error up
                                return Err(err.into());
                            }
                            broadcast::error::RecvError::Lagged(lagged) => {
                                // querying the block was delayed
                                // this can happen if tokio is very busy or waiting for requests limits took too long
                                if self.backup {
                                    debug!("http interval on {} lagging by {}!", self, lagged);
                                } else {
                                    warn!("http interval on {} lagging by {}!", self, lagged);
                                }
                            }
                        }
                    }
                }
            }
            Web3Provider::Both(_, client) | Web3Provider::Ws(client) => {
                // todo: move subscribe_blocks onto the request handle?
                let active_request_handle = self
                    .wait_for_request_handle(&authorization, None, Some(provider.clone()))
                    .await;
                let mut stream = client.subscribe_blocks().await?;
                drop(active_request_handle);

                // query the block once since the subscription doesn't send the current block
                // there is a very small race condition here where the stream could send us a new block right now
                // but all that does is print "new block" for the same block as current block
                // TODO: how does this get wrapped in an arc? does ethers handle that?
                // TODO: do this part over http?
                let block: Result<Option<ArcBlock>, _> = self
                    .wait_for_request_handle(&authorization, None, Some(provider.clone()))
                    .await?
                    .request(
                        "eth_getBlockByNumber",
                        &json!(("latest", false)),
                        Level::Warn.into(),
                        Some(provider.clone()),
                    )
                    .await;

                let mut last_hash = match &block {
                    Ok(Some(new_block)) => new_block
                        .hash
                        .expect("blocks should always have a hash here"),
                    _ => H256::zero(),
                };

                self.send_head_block_result(block, &block_sender, block_map.clone())
                    .await?;

                while let Some(new_block) = stream.next().await {
                    // TODO: select on disconnect_watch instead of waiting for a block to arrive
                    if self.should_disconnect() {
                        break;
                    }

                    // TODO: check the new block's hash to be sure we don't send dupes
                    let new_hash = new_block
                        .hash
                        .expect("blocks should always have a hash here");

                    if new_hash == last_hash {
                        // some rpcs like to give us duplicates. don't waste our time on them
                        continue;
                    } else {
                        last_hash = new_hash;
                    }

                    self.send_head_block_result(
                        Ok(Some(Arc::new(new_block))),
                        &block_sender,
                        block_map.clone(),
                    )
                    .await?;
                }

                // TODO: is this always an error?
                // TODO: we probably don't want a warn and to return error
                debug!("new_heads subscription to {} ended", self);
            }
            #[cfg(test)]
            Web3Provider::Mock => unimplemented!(),
        }

        // clear the head block. this might not be needed, but it won't hurt
        self.send_head_block_result(Ok(None), &block_sender, block_map)
            .await?;

        if self.should_disconnect() {
            Ok(())
        } else {
            Err(anyhow!("new_heads subscription exited. reconnect needed"))
        }
    }

    /// Turn on the firehose of pending transactions
    async fn subscribe_pending_transactions(
        self: Arc<Self>,
        authorization: Arc<Authorization>,
        tx_id_sender: flume::Sender<(TxHash, Arc<Self>)>,
    ) -> anyhow::Result<()> {
        // TODO: give this a separate client. don't use new_head_client for everything. especially a firehose this big
        // TODO: timeout
        let provider = self.wait_for_provider().await;

        trace!("watching pending transactions on {}", self);
        // TODO: does this keep the lock open for too long?
        match provider.as_ref() {
            Web3Provider::Http(_provider) => {
                // there is a "watch_pending_transactions" function, but a lot of public nodes do not support the necessary rpc endpoints
                self.wait_for_disconnect().await?;
            }
            Web3Provider::Both(_, client) | Web3Provider::Ws(client) => {
                // TODO: maybe the subscribe_pending_txs function should be on the active_request_handle
                let active_request_handle = self
                    .wait_for_request_handle(&authorization, None, Some(provider.clone()))
                    .await?;

                let mut stream = client.subscribe_pending_txs().await?;

                drop(active_request_handle);

                while let Some(pending_tx_id) = stream.next().await {
                    tx_id_sender
                        .send_async((pending_tx_id, self.clone()))
                        .await
                        .context("tx_id_sender")?;

                    // TODO: periodically check for listeners. if no one is subscribed, unsubscribe and wait for a subscription

                    // TODO: select on this instead of checking every loop
                    if self.should_disconnect() {
                        break;
                    }
                }

                // TODO: is this always an error?
                // TODO: we probably don't want a warn and to return error
                debug!("pending_transactions subscription ended on {}", self);
            }
            #[cfg(test)]
            Web3Provider::Mock => {
                self.wait_for_disconnect().await?;
            }
        }

        if self.should_disconnect() {
            Ok(())
        } else {
            Err(anyhow!(
                "pending_transactions subscription exited. reconnect needed"
            ))
        }
    }

    /// be careful with this; it might wait forever!
    /// `allow_not_ready` is only for use by health checks while starting the provider
    /// TODO: don't use anyhow. use specific error type
    pub async fn wait_for_request_handle<'a>(
        self: &'a Arc<Self>,
        authorization: &'a Arc<Authorization>,
        max_wait: Option<Duration>,
        unlocked_provider: Option<Arc<Web3Provider>>,
    ) -> Web3ProxyResult<OpenRequestHandle> {
        let max_wait = max_wait.map(|x| Instant::now() + x);

        loop {
            match self
                .try_request_handle(authorization, unlocked_provider.clone())
                .await
            {
                Ok(OpenRequestResult::Handle(handle)) => return Ok(handle),
                Ok(OpenRequestResult::RetryAt(retry_at)) => {
                    // TODO: emit a stat?
                    let wait = retry_at.duration_since(Instant::now());

                    trace!(
                        "waiting {} millis for request handle on {}",
                        wait.as_millis(),
                        self
                    );

                    if let Some(max_wait) = max_wait {
                        if retry_at > max_wait {
                            // break now since we will wait past our maximum wait time
                            return Err(Web3ProxyError::Timeout(None));
                        }
                    }

                    sleep_until(retry_at).await;
                }
                Ok(OpenRequestResult::NotReady) => {
                    // TODO: when can this happen? log? emit a stat?
                    trace!("{} has no handle ready", self);

                    if let Some(max_wait) = max_wait {
                        let now = Instant::now();

                        if now > max_wait {
                            return Err(Web3ProxyError::NoHandleReady);
                        }
                    }

                    // TODO: sleep how long? maybe just error?
                    // TODO: instead of an arbitrary sleep, subscribe to the head block on this
                    sleep(Duration::from_millis(10)).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    pub async fn try_request_handle(
        self: &Arc<Self>,
        authorization: &Arc<Authorization>,
        // TODO: borrow on this instead of needing to clone the Arc?
        unlocked_provider: Option<Arc<Web3Provider>>,
    ) -> Web3ProxyResult<OpenRequestResult> {
        // TODO: think more about this read block
        // TODO: this should *not* be new_head_client. this should be a separate object
        if unlocked_provider.is_some() || self.provider.read().await.is_some() {
            // we already have an unlocked provider. no need to lock
        } else {
            return Ok(OpenRequestResult::NotReady);
        }

        if let Some(hard_limit_until) = self.hard_limit_until.as_ref() {
            let hard_limit_ready = *hard_limit_until.borrow();
            let now = Instant::now();
            if now < hard_limit_ready {
                return Ok(OpenRequestResult::RetryAt(hard_limit_ready));
            }
        }

        // check rate limits
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
                    if !self.backup {
                        let when = retry_at.duration_since(Instant::now());
                        warn!(
                            "Exhausted rate limit on {}. Retry in {}ms",
                            self,
                            when.as_millis()
                        );
                    }

                    if let Some(hard_limit_until) = self.hard_limit_until.as_ref() {
                        hard_limit_until.send_replace(retry_at);
                    }

                    return Ok(OpenRequestResult::RetryAt(retry_at));
                }
                RedisRateLimitResult::RetryNever => {
                    return Ok(OpenRequestResult::NotReady);
                }
            }
        };

        let handle = OpenRequestHandle::new(authorization.clone(), self.clone()).await;

        Ok(OpenRequestResult::Handle(handle))
    }

    async fn wait_for_disconnect(&self) -> Result<(), tokio::sync::watch::error::RecvError> {
        let mut disconnect_watch = self.disconnect_watch.as_ref().unwrap().subscribe();

        loop {
            if *disconnect_watch.borrow_and_update() {
                // disconnect watch is set to "true"
                return Ok(());
            }

            // wait for disconnect_watch to change
            disconnect_watch.changed().await?;
        }
    }

    async fn wait_for_provider(&self) -> Arc<Web3Provider> {
        let mut provider = self.provider.read().await.clone();

        let mut logged = false;
        while provider.is_none() {
            // trace!("waiting on unlocked_provider: locking...");
            sleep(Duration::from_millis(100)).await;

            if !logged {
                debug!("waiting for provider on {}", self);
                logged = true;
            }

            provider = self.provider.read().await.clone();
        }

        provider.unwrap()
    }

    pub async fn wait_for_query<P, R>(
        self: &Arc<Self>,
        method: &str,
        params: &P,
        revert_handler: RequestErrorHandler,
        authorization: Arc<Authorization>,
        unlocked_provider: Option<Arc<Web3Provider>>,
    ) -> anyhow::Result<R>
    where
        // TODO: not sure about this type. would be better to not need clones, but measure and spawns combine to need it
        P: Clone + fmt::Debug + serde::Serialize + Send + Sync + 'static,
        R: serde::Serialize + serde::de::DeserializeOwned + fmt::Debug + Send,
    {
        self.wait_for_request_handle(&authorization, None, None)
            .await?
            .request::<P, R>(method, params, revert_handler, unlocked_provider)
            .await
            .context("ProviderError from the backend")
    }
}

impl fmt::Debug for Web3Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default Debug takes forever to write. this is too quiet though. we at least need the url
        f.debug_struct("Web3Provider").finish_non_exhaustive()
    }
}

impl Hash for Web3Rpc {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.display_name.hash(state);
        self.http_url.hash(state);
        self.ws_url.hash(state);
        self.automatic_block_limit.hash(state);
        self.backup.hash(state);
        // TODO: including soft_limit might need to change if we change them to be dynamic
        self.soft_limit.hash(state);
        self.tier.hash(state);
        self.created_at.hash(state);
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
        // 3 is the number of fields in the struct.
        let mut state = serializer.serialize_struct("Web3Rpc", 9)?;

        // the url is excluded because it likely includes private information. just show the name that we use in keys
        state.serialize_field("name", &self.name)?;
        // a longer name for display to users
        state.serialize_field("display_name", &self.display_name)?;

        state.serialize_field("backup", &self.backup)?;

        match self.block_data_limit.load(atomic::Ordering::Relaxed) {
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
        state.serialize_field("head_block", &*self.head_block.read())?;

        state.serialize_field("head_latency", &self.head_latency.read().value())?;

        state.serialize_field(
            "total_requests",
            &self.total_requests.load(atomic::Ordering::Relaxed),
        )?;

        state.end()
    }
}

impl fmt::Debug for Web3Rpc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut f = f.debug_struct("Web3Rpc");

        f.field("name", &self.name);

        let block_data_limit = self.block_data_limit.load(atomic::Ordering::Relaxed);
        if block_data_limit == u64::MAX {
            f.field("blocks", &"all");
        } else {
            f.field("blocks", &block_data_limit);
        }

        f.finish_non_exhaustive()
    }
}

impl fmt::Display for Web3Rpc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: filter basic auth and api keys
        write!(f, "{}", &self.name)
    }
}

mod tests {
    #![allow(unused_imports)]
    use super::*;
    use ethers::types::{Block, U256};

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

        let x = Web3Rpc {
            name: "name".to_string(),
            ws_url: Some("ws://example.com".to_string()),
            soft_limit: 1_000,
            automatic_block_limit: false,
            backup: false,
            block_data_limit: block_data_limit.into(),
            tier: 0,
            head_block: RwLock::new(Some(head_block.clone())),
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

        let x = Web3Rpc {
            name: "name".to_string(),
            soft_limit: 1_000,
            automatic_block_limit: false,
            backup: false,
            block_data_limit: block_data_limit.into(),
            tier: 0,
            head_block: RwLock::new(Some(head_block.clone())),
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
            head_block: RwLock::new(Some(head_block.clone())),
        };

        assert!(!x.has_block_data(&0.into()));
        assert!(!x.has_block_data(&1.into()));
        assert!(!x.has_block_data(&head_block.number()));
        assert!(!x.has_block_data(&(head_block.number() + 1)));
        assert!(!x.has_block_data(&(head_block.number() + 1000)));
    }
    */
}
