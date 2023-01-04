///! Rate-limited communication with a web3 provider.
use super::blockchain::{ArcBlock, BlockHashesCache, SavedBlock};
use super::provider::Web3Provider;
use super::request::{OpenRequestHandle, OpenRequestHandleMetrics, OpenRequestResult};
use crate::app::{flatten_handle, AnyhowJoinHandle};
use crate::config::BlockAndRpc;
use crate::frontend::authorization::Authorization;
use anyhow::Context;
use ethers::prelude::{Bytes, Middleware, ProviderError, TxHash, H256, U64};
use ethers::types::U256;
use futures::future::try_join_all;
use futures::StreamExt;
use log::{debug, error, info, trace, warn, Level};
use migration::sea_orm::DatabaseConnection;
use parking_lot::RwLock;
use redis_rate_limiter::{RedisPool, RedisRateLimitResult, RedisRateLimiter};
use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;
use serde_json::json;
use std::cmp::min;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{self, AtomicU32, AtomicU64};
use std::{cmp::Ordering, sync::Arc};
use thread_fast_rng::rand::Rng;
use thread_fast_rng::thread_fast_rng;
use tokio::sync::{broadcast, oneshot, RwLock as AsyncRwLock};
use tokio::time::{interval, sleep, sleep_until, timeout, Duration, Instant, MissedTickBehavior};

// TODO: maybe provider state should have the block data limit in it. but it is inside an async lock and we can't Serialize then
#[derive(Clone, Debug)]
pub enum ProviderState {
    None,
    NotReady(Arc<Web3Provider>),
    Ready(Arc<Web3Provider>),
}

impl ProviderState {
    pub async fn provider(&self, allow_not_ready: bool) -> Option<&Arc<Web3Provider>> {
        match self {
            ProviderState::None => None,
            ProviderState::NotReady(x) => {
                if allow_not_ready {
                    Some(x)
                } else {
                    // TODO: do a ready check here?
                    None
                }
            }
            ProviderState::Ready(x) => {
                if x.ready() {
                    Some(x)
                } else {
                    None
                }
            }
        }
    }
}

/// An active connection to a Web3 RPC server like geth or erigon.
pub struct Web3Connection {
    pub name: String,
    pub display_name: Option<String>,
    pub db_conn: Option<DatabaseConnection>,
    /// TODO: can we get this from the provider? do we even need it?
    pub(super) url: String,
    /// Some connections use an http_client. we keep a clone for reconnecting
    pub(super) http_client: Option<reqwest::Client>,
    /// keep track of currently open requests. We sort on this
    pub(super) active_requests: AtomicU32,
    /// keep track of total requests from the frontend
    pub(super) frontend_requests: AtomicU64,
    /// keep track of total requests from web3-proxy itself
    pub(super) internal_requests: AtomicU64,
    /// provider is in a RwLock so that we can replace it if re-connecting
    /// it is an async lock because we hold it open across awaits
    pub(super) provider_state: AsyncRwLock<ProviderState>,
    /// rate limits are stored in a central redis so that multiple proxies can share their rate limits
    /// We do not use the deferred rate limiter because going over limits would cause errors
    pub(super) hard_limit: Option<RedisRateLimiter>,
    /// used for load balancing to the least loaded server
    pub(super) soft_limit: u32,
    /// use web3 queries to find the block data limit for archive/pruned nodes
    pub(super) automatic_block_limit: bool,
    /// TODO: have an enum for this so that "no limit" prints pretty?
    pub(super) block_data_limit: AtomicU64,
    /// Lower tiers are higher priority when sending requests
    pub(super) tier: u64,
    /// TODO: should this be an AsyncRwLock?
    pub(super) head_block: RwLock<Option<SavedBlock>>,
    pub(super) open_request_handle_metrics: Arc<OpenRequestHandleMetrics>,
}

impl Web3Connection {
    /// Connect to a web3 rpc
    // TODO: have this take a builder (which will have channels attached). or maybe just take the config and give the config public fields
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn(
        name: String,
        display_name: Option<String>,
        chain_id: u64,
        db_conn: Option<DatabaseConnection>,
        url_str: String,
        // optional because this is only used for http providers. websocket providers don't use it
        http_client: Option<reqwest::Client>,
        http_interval_sender: Option<Arc<broadcast::Sender<()>>>,
        // TODO: have a builder struct for this.
        hard_limit: Option<(u64, RedisPool)>,
        // TODO: think more about this type
        soft_limit: u32,
        block_data_limit: Option<u64>,
        block_map: BlockHashesCache,
        block_sender: Option<flume::Sender<BlockAndRpc>>,
        tx_id_sender: Option<flume::Sender<(TxHash, Arc<Self>)>>,
        reconnect: bool,
        tier: u64,
        open_request_handle_metrics: Arc<OpenRequestHandleMetrics>,
    ) -> anyhow::Result<(Arc<Web3Connection>, AnyhowJoinHandle<()>)> {
        let hard_limit = hard_limit.map(|(hard_rate_limit, redis_pool)| {
            // TODO: is cache size 1 okay? i think we need
            RedisRateLimiter::new(
                "web3_proxy",
                &format!("{}:{}", chain_id, name),
                hard_rate_limit,
                60.0,
                redis_pool,
            )
        });

        // TODO: should we do this even if block_sender is None? then we would know limits on private relays
        let block_data_limit: AtomicU64 = block_data_limit.unwrap_or_default().into();
        let automatic_block_limit =
            (block_data_limit.load(atomic::Ordering::Acquire) == 0) && block_sender.is_some();

        let new_connection = Self {
            name,
            db_conn: db_conn.clone(),
            display_name,
            http_client,
            url: url_str,
            active_requests: 0.into(),
            frontend_requests: 0.into(),
            internal_requests: 0.into(),
            provider_state: AsyncRwLock::new(ProviderState::None),
            hard_limit,
            soft_limit,
            automatic_block_limit,
            block_data_limit,
            head_block: RwLock::new(Default::default()),
            tier,
            open_request_handle_metrics,
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
                        http_interval_sender,
                        reconnect,
                        tx_id_sender,
                    )
                    .await
            })
        };

        Ok((new_connection, handle))
    }

    // TODO: would be great if rpcs exposed this. see https://github.com/ledgerwatch/erigon/issues/6391
    async fn check_block_data_limit(
        self: &Arc<Self>,
        authorization: &Arc<Authorization>,
    ) -> anyhow::Result<Option<u64>> {
        if !self.automatic_block_limit {
            // TODO: is this a good thing to return?
            return Ok(None);
        }

        // check if we are synced
        let head_block: ArcBlock = self
            .wait_for_request_handle(authorization, Duration::from_secs(30), true)
            .await?
            .request::<_, Option<_>>(
                "eth_getBlockByNumber",
                &json!(("latest", false)),
                // error here are expected, so keep the level low
                Level::Warn.into(),
            )
            .await?
            .context("no block during check_block_data_limit!")?;

        if SavedBlock::from(head_block).syncing(60) {
            // if the node is syncing, we can't check its block data limit
            return Ok(None);
        }

        // TODO: add SavedBlock to self? probably best not to. we might not get marked Ready

        let mut limit = None;

        // TODO: binary search between 90k and max?
        // TODO: start at 0 or 1?
        for block_data_limit in [0, 32, 64, 128, 256, 512, 1024, 90_000, u64::MAX] {
            let handle = self
                .wait_for_request_handle(authorization, Duration::from_secs(30), true)
                .await?;

            let head_block_num_future = handle.request::<Option<()>, U256>(
                "eth_blockNumber",
                &None,
                // error here are expected, so keep the level low
                Level::Debug.into(),
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
                .wait_for_request_handle(authorization, Duration::from_secs(30), true)
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

        info!("block data limit on {}: {:?}", self, limit);

        Ok(limit)
    }

    /// TODO: this might be too simple. different nodes can prune differently. its possible we will have a block range
    pub fn block_data_limit(&self) -> U64 {
        self.block_data_limit.load(atomic::Ordering::Acquire).into()
    }

    pub fn syncing(&self) -> bool {
        match self.head_block.read().clone() {
            None => true,
            Some(x) => x.syncing(60),
        }
    }

    pub fn has_block_data(&self, needed_block_num: &U64) -> bool {
        let head_block_num = match self.head_block.read().clone() {
            None => return false,
            Some(x) => {
                if x.syncing(60) {
                    // skip syncing nodes. even though they might be able to serve a query,
                    // latency will be poor and it will get in the way of them syncing further
                    return false;
                }

                x.number()
            }
        };

        // this rpc doesn't have that block yet. still syncing
        if needed_block_num > &head_block_num {
            return false;
        }

        // if this is a pruning node, we might not actually have the block
        let block_data_limit: U64 = self.block_data_limit();

        let oldest_block_num = head_block_num.saturating_sub(block_data_limit);

        *needed_block_num >= oldest_block_num
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

            warn!("Reconnect to {} in {}ms", self, reconnect_in.as_millis());

            sleep(reconnect_in).await;

            first_sleep_ms
        } else {
            base_ms
        };

        // retry until we succeed
        while let Err(err) = self.connect(block_sender, chain_id, db_conn).await {
            // thread_rng is crytographically secure. we don't need that here
            sleep_ms = min(
                cap_ms,
                thread_fast_rng().gen_range(base_ms..(sleep_ms * range_multiplier)),
            );

            let retry_in = Duration::from_millis(sleep_ms);
            warn!(
                "Failed reconnect to {}! Retry in {}ms. err={:?}",
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
        // trace!("provider_state {} locking...", self);
        let mut provider_state = self
            .provider_state
            .try_write()
            .context("locking provider for write")?;
        // trace!("provider_state {} locked: {:?}", self, provider_state);

        match &*provider_state {
            ProviderState::None => {
                info!("connecting to {}", self);
            }
            ProviderState::NotReady(provider) | ProviderState::Ready(provider) => {
                // disconnect the current provider
                if let Web3Provider::Mock = provider.as_ref() {
                    return Ok(());
                }

                debug!("reconnecting to {}", self);

                // disconnect the current provider
                *provider_state = ProviderState::None;

                // reset sync status
                // trace!("locking head block on {}", self);
                {
                    let mut head_block = self.head_block.write();
                    *head_block = None;
                }
                // trace!("done with head block on {}", self);

                // tell the block subscriber that we don't have any blocks
                if let Some(block_sender) = block_sender {
                    block_sender
                        .send_async((None, self.clone()))
                        .await
                        .context("block_sender during connect")?;
                }
            }
        }

        // trace!("Creating new Web3Provider on {}", self);
        // TODO: if this fails, keep retrying! otherwise it crashes and doesn't try again!
        let new_provider = Web3Provider::from_str(&self.url, self.http_client.clone()).await?;

        // trace!("saving provider state as NotReady on {}", self);
        *provider_state = ProviderState::NotReady(Arc::new(new_provider));

        // drop the lock so that we can get a request handle
        // trace!("provider_state {} unlocked", self);
        drop(provider_state);

        let authorization = Arc::new(Authorization::internal(db_conn.cloned())?);

        // check the server's chain_id here
        // TODO: some public rpcs (on bsc and fantom) do not return an id and so this ends up being an error
        // TODO: what should the timeout be? should there be a request timeout?
        // trace!("waiting on chain id for {}", self);
        let found_chain_id: Result<U64, _> = self
            .wait_for_request_handle(&authorization, Duration::from_secs(30), true)
            .await?
            .request(
                "eth_chainId",
                &json!(Option::None::<()>),
                Level::Trace.into(),
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
                return Err(anyhow::Error::from(e));
            }
        }

        self.check_block_data_limit(&authorization).await?;

        {
            // trace!("locking for ready...");
            let mut provider_state = self.provider_state.write().await;
            // trace!("locked for ready...");

            // TODO: do this without a clone
            let ready_provider = provider_state
                .provider(true)
                .await
                .context("provider missing")?
                .clone();

            *provider_state = ProviderState::Ready(ready_provider);
            // trace!("unlocked for ready...");
        }

        info!("successfully connected to {}", self);

        Ok(())
    }

    #[inline]
    pub fn active_requests(&self) -> u32 {
        self.active_requests.load(atomic::Ordering::Acquire)
    }

    async fn send_head_block_result(
        self: &Arc<Self>,
        new_head_block: Result<Option<ArcBlock>, ProviderError>,
        block_sender: &flume::Sender<BlockAndRpc>,
        block_map: BlockHashesCache,
    ) -> anyhow::Result<()> {
        let new_head_block = match new_head_block {
            Ok(None) => {
                {
                    let mut head_block = self.head_block.write();

                    if head_block.is_none() {
                        // we previously sent a None. return early
                        return Ok(());
                    }
                    warn!("{} is not synced!", self);

                    *head_block = None;
                }

                None
            }
            Ok(Some(new_head_block)) => {
                let new_hash = new_head_block
                    .hash
                    .context("sending block to connections")?;

                // if we already have this block saved, set new_head_block to that arc. otherwise store this copy
                let new_head_block = block_map
                    .get_with(new_hash, async move { new_head_block })
                    .await;

                // save the block so we don't send the same one multiple times
                // also save so that archive checks can know how far back to query
                {
                    let mut head_block = self.head_block.write();

                    let _ = head_block.insert(new_head_block.clone().into());
                }

                if self.block_data_limit() == U64::zero() && !self.syncing() {
                    let authorization = Arc::new(Authorization::internal(self.db_conn.clone())?);
                    if let Err(err) = self.check_block_data_limit(&authorization).await {
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

    /// subscribe to blocks and transactions with automatic reconnects
    /// This should only exit when the program is exiting.
    /// TODO: should more of these args be on self?
    #[allow(clippy::too_many_arguments)]
    async fn subscribe(
        self: Arc<Self>,
        authorization: &Arc<Authorization>,
        block_map: BlockHashesCache,
        block_sender: Option<flume::Sender<BlockAndRpc>>,
        chain_id: u64,
        http_interval_sender: Option<Arc<broadcast::Sender<()>>>,
        reconnect: bool,
        tx_id_sender: Option<flume::Sender<(TxHash, Arc<Self>)>>,
    ) -> anyhow::Result<()> {
        loop {
            let http_interval_receiver = http_interval_sender.as_ref().map(|x| x.subscribe());

            let mut futures = vec![];

            {
                // health check
                // TODO: move this into a proper function
                let authorization = authorization.clone();
                let block_sender = block_sender.clone();
                let conn = self.clone();
                let (ready_tx, ready_rx) = oneshot::channel();
                let f = async move {
                    // initial sleep to allow for the initial connection
                    conn.retrying_connect(
                        block_sender.as_ref(),
                        chain_id,
                        authorization.db_conn.as_ref(),
                        false,
                    )
                    .await?;

                    // provider is ready
                    ready_tx.send(()).unwrap();

                    // wait before doing the initial health check
                    // TODO: how often?
                    // TODO: subscribe to self.head_block
                    let health_sleep_seconds = 10;
                    sleep(Duration::from_secs(health_sleep_seconds)).await;

                    let mut warned = 0;

                    loop {
                        // TODO: what if we just happened to have this check line up with another restart?
                        // TODO: think more about this
                        // trace!("health check on {}. locking...", conn);
                        if conn
                            .provider_state
                            .read()
                            .await
                            .provider(false)
                            .await
                            .is_none()
                        {
                            // trace!("health check unlocked with error on {}", conn);
                            // returning error will trigger a reconnect
                            return Err(anyhow::anyhow!("{} is not ready", conn));
                        }
                        // trace!("health check on {}. unlocked", conn);

                        if let Some(x) = &*conn.head_block.read() {
                            // if this block is too old, return an error so we reconnect
                            let current_lag = x.lag();
                            if current_lag > 0 {
                                let level = if warned == 0 {
                                    log::Level::Warn
                                } else if current_lag % 1000 == 0 {
                                    log::Level::Debug
                                } else {
                                    log::Level::Trace
                                };

                                log::log!(
                                    level,
                                    "{} is lagged {} secs: {} {}",
                                    conn,
                                    current_lag,
                                    x.number(),
                                    x.hash(),
                                );

                                warned += 1;
                            } else {
                                // reset warnings now that we are connected
                                warned = 0;
                            }
                        }

                        sleep(Duration::from_secs(health_sleep_seconds)).await;
                    }
                };

                futures.push(flatten_handle(tokio::spawn(f)));

                // wait on the initial connection
                ready_rx.await?;
            }

            if let Some(block_sender) = &block_sender {
                let f = self.clone().subscribe_new_heads(
                    authorization.clone(),
                    http_interval_receiver,
                    block_sender.clone(),
                    block_map.clone(),
                );

                futures.push(flatten_handle(tokio::spawn(f)));
            }

            if let Some(tx_id_sender) = &tx_id_sender {
                let f = self
                    .clone()
                    .subscribe_pending_transactions(authorization.clone(), tx_id_sender.clone());

                futures.push(flatten_handle(tokio::spawn(f)));
            }

            match try_join_all(futures).await {
                Ok(_) => {
                    // futures all exited without error. break instead of restarting subscriptions
                    break;
                }
                Err(err) => {
                    if reconnect {
                        warn!("{} connection ended. err={:?}", self, err);

                        self.clone()
                            .retrying_connect(
                                block_sender.as_ref(),
                                chain_id,
                                authorization.db_conn.as_ref(),
                                true,
                            )
                            .await?;
                    } else {
                        error!("{} subscription exited. err={:?}", self, err);
                        return Err(err);
                    }
                }
            }
        }

        info!("all subscriptions on {} completed", self);

        Ok(())
    }

    /// Subscribe to new blocks. If `reconnect` is true, this runs forever.
    async fn subscribe_new_heads(
        self: Arc<Self>,
        authorization: Arc<Authorization>,
        http_interval_receiver: Option<broadcast::Receiver<()>>,
        block_sender: flume::Sender<BlockAndRpc>,
        block_map: BlockHashesCache,
    ) -> anyhow::Result<()> {
        trace!("watching new heads on {}", self);

        // trace!("locking on new heads");
        let provider_state = self
            .provider_state
            .try_read()
            .context("subscribe_new_heads")?
            .clone();
        // trace!("unlocked on new heads");

        // TODO: need a timeout
        if let ProviderState::Ready(provider) = provider_state {
            match provider.as_ref() {
                Web3Provider::Mock => unimplemented!(),
                Web3Provider::Http(_provider) => {
                    // there is a "watch_blocks" function, but a lot of public nodes do not support the necessary rpc endpoints
                    // TODO: try watch_blocks and fall back to this?

                    let mut http_interval_receiver = http_interval_receiver.unwrap();

                    let mut last_hash = H256::zero();

                    loop {
                        // TODO: what should the max_wait be?
                        match self
                            .wait_for_request_handle(&authorization, Duration::from_secs(30), false)
                            .await
                        {
                            Ok(active_request_handle) => {
                                let block: Result<Option<ArcBlock>, _> = active_request_handle
                                    .request(
                                        "eth_getBlockByNumber",
                                        &json!(("latest", false)),
                                        Level::Warn.into(),
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
                                        let new_hash = block
                                            .hash
                                            .expect("blocks here should always have hashes");

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

                                self.send_head_block_result(
                                    Ok(None),
                                    &block_sender,
                                    block_map.clone(),
                                )
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
                                    warn!("http interval on {} lagging by {}!", self, lagged);
                                }
                            }
                        }
                    }
                }
                Web3Provider::Ws(provider) => {
                    // todo: move subscribe_blocks onto the request handle?
                    let active_request_handle = self
                        .wait_for_request_handle(&authorization, Duration::from_secs(30), false)
                        .await;
                    let mut stream = provider.subscribe_blocks().await?;
                    drop(active_request_handle);

                    // query the block once since the subscription doesn't send the current block
                    // there is a very small race condition here where the stream could send us a new block right now
                    // all it does is print "new block" for the same block as current block
                    // TODO: how does this get wrapped in an arc? does ethers handle that?
                    let block: Result<Option<ArcBlock>, _> = self
                        .wait_for_request_handle(&authorization, Duration::from_secs(30), false)
                        .await?
                        .request(
                            "eth_getBlockByNumber",
                            &json!(("latest", false)),
                            Level::Warn.into(),
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

                    // clear the head block. this might not be needed, but it won't hurt
                    self.send_head_block_result(Ok(None), &block_sender, block_map)
                        .await?;

                    // TODO: is this always an error?
                    // TODO: we probably don't want a warn and to return error
                    warn!("new_heads subscription to {} ended", self);
                    Err(anyhow::anyhow!("new_heads subscription ended"))
                }
            }
        } else {
            Err(anyhow::anyhow!(
                "Provider not ready! Unable to subscribe to heads"
            ))
        }
    }

    async fn subscribe_pending_transactions(
        self: Arc<Self>,
        authorization: Arc<Authorization>,
        tx_id_sender: flume::Sender<(TxHash, Arc<Self>)>,
    ) -> anyhow::Result<()> {
        if let ProviderState::Ready(provider) = self
            .provider_state
            .try_read()
            .context("subscribe_pending_transactions")?
            .clone()
        {
            trace!("watching pending transactions on {}", self);
            match provider.as_ref() {
                Web3Provider::Mock => unimplemented!(),
                Web3Provider::Http(provider) => {
                    // there is a "watch_pending_transactions" function, but a lot of public nodes do not support the necessary rpc endpoints
                    // TODO: what should this interval be? probably automatically set to some fraction of block time
                    // TODO: maybe it would be better to have one interval for all of the http providers, but this works for now
                    // TODO: if there are some websocket providers, maybe have a longer interval and a channel that tells the https to update when a websocket gets a new head? if they are slow this wouldn't work well though
                    let mut interval = interval(Duration::from_secs(60));
                    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

                    loop {
                        // TODO: actually do something here
                        /*
                        match self.try_request_handle().await {
                            Ok(active_request_handle) => {
                                // TODO: check the filter
                                todo!("actually send a request");
                            }
                            Err(e) => {
                                warn!("Failed getting latest block from {}: {:?}", self, e);
                            }
                        }
                        */

                        // wait for the interval
                        // TODO: if error or rate limit, increase interval?
                        interval.tick().await;
                    }
                }
                Web3Provider::Ws(provider) => {
                    // TODO: maybe the subscribe_pending_txs function should be on the active_request_handle
                    let active_request_handle = self
                        .wait_for_request_handle(&authorization, Duration::from_secs(30), false)
                        .await;

                    let mut stream = provider.subscribe_pending_txs().await?;

                    drop(active_request_handle);

                    while let Some(pending_tx_id) = stream.next().await {
                        tx_id_sender
                            .send_async((pending_tx_id, self.clone()))
                            .await
                            .context("tx_id_sender")?;

                        // TODO: periodically check for listeners. if no one is subscribed, unsubscribe and wait for a subscription
                    }

                    // TODO: is this always an error?
                    // TODO: we probably don't want a warn and to return error
                    warn!("pending_transactions subscription ended on {}", self);
                    return Err(anyhow::anyhow!("pending_transactions subscription ended"));
                }
            }
        } else {
            warn!(
                "Provider not ready! Unable to watch pending transactions on {}",
                self
            );
        }

        Ok(())
    }

    /// be careful with this; it might wait forever!
    /// `allow_not_ready` is only for use by health checks while starting the provider
    pub async fn wait_for_request_handle(
        self: &Arc<Self>,
        authorization: &Arc<Authorization>,
        max_wait: Duration,
        allow_not_ready: bool,
    ) -> anyhow::Result<OpenRequestHandle> {
        let max_wait = Instant::now() + max_wait;

        loop {
            match self
                .try_request_handle(authorization, allow_not_ready)
                .await
            {
                Ok(OpenRequestResult::Handle(handle)) => return Ok(handle),
                Ok(OpenRequestResult::RetryAt(retry_at)) => {
                    // TODO: emit a stat?
                    // // trace!(?retry_at);

                    if retry_at > max_wait {
                        // break now since we will wait past our maximum wait time
                        // TODO: don't use anyhow. use specific error type
                        return Err(anyhow::anyhow!("timeout waiting for request handle"));
                    }
                    sleep_until(retry_at).await;
                }
                Ok(OpenRequestResult::NotReady) => {
                    // TODO: when can this happen? log? emit a stat?
                    // TODO: subscribe to the head block on this
                    // TODO: sleep how long? maybe just error?
                    // TODO: don't use anyhow. use specific error type
                    return Err(anyhow::anyhow!("unable to retry for request handle"));
                }
                Err(err) => return Err(err),
            }
        }
    }

    pub async fn try_request_handle(
        self: &Arc<Self>,
        authorization: &Arc<Authorization>,
        // TODO? ready_provider: Option<&Arc<Web3Provider>>,
        allow_not_ready: bool,
    ) -> anyhow::Result<OpenRequestResult> {
        // TODO: think more about this read block
        if !allow_not_ready
            && self
                .provider_state
                .read()
                .await
                .provider(allow_not_ready)
                .await
                .is_none()
        {
            return Ok(OpenRequestResult::NotReady);
        }

        // check rate limits
        if let Some(ratelimiter) = self.hard_limit.as_ref() {
            // TODO: how should we know if we should set expire or not?
            match ratelimiter.throttle().await? {
                RedisRateLimitResult::Allowed(_) => {
                    // // trace!("rate limit succeeded")
                }
                RedisRateLimitResult::RetryAt(retry_at, _) => {
                    // rate limit failed
                    // save the smallest retry_after. if nothing succeeds, return an Err with retry_after in it
                    // TODO: use tracing better
                    // TODO: i'm seeing "Exhausted rate limit on moralis: 0ns". How is it getting 0?
                    warn!("Exhausted rate limit on {}. Retry at {:?}", self, retry_at);

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
}

impl fmt::Debug for Web3Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default Debug takes forever to write. this is too quiet though. we at least need the url
        f.debug_struct("Web3Provider").finish_non_exhaustive()
    }
}

impl Hash for Web3Connection {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // TODO: is this enough?
        self.name.hash(state);
    }
}

impl Eq for Web3Connection {}

impl Ord for Web3Connection {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.name.cmp(&other.name)
    }
}

impl PartialOrd for Web3Connection {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Web3Connection {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Serialize for Web3Connection {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // 3 is the number of fields in the struct.
        let mut state = serializer.serialize_struct("Web3Connection", 9)?;

        // the url is excluded because it likely includes private information. just show the name that we use in keys
        state.serialize_field("name", &self.name)?;
        // a longer name for display to users
        state.serialize_field("display_name", &self.display_name)?;

        match self.block_data_limit.load(atomic::Ordering::Relaxed) {
            u64::MAX => {
                state.serialize_field("block_data_limit", &None::<()>)?;
            }
            block_data_limit => {
                state.serialize_field("block_data_limit", &block_data_limit)?;
            }
        }

        state.serialize_field("tier", &self.tier)?;
        state.serialize_field("weight", &1.0)?;

        state.serialize_field("soft_limit", &self.soft_limit)?;

        state.serialize_field(
            "active_requests",
            &self.active_requests.load(atomic::Ordering::Relaxed),
        )?;

        state.serialize_field(
            "total_requests",
            &self.frontend_requests.load(atomic::Ordering::Relaxed),
        )?;

        {
            // TODO: maybe this is too much data. serialize less?
            let head_block = &*self.head_block.read();
            state.serialize_field("head_block", head_block)?;
        }

        state.end()
    }
}

impl fmt::Debug for Web3Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut f = f.debug_struct("Web3Connection");

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

impl fmt::Display for Web3Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: filter basic auth and api keys
        write!(f, "{}", &self.name)
    }
}

mod tests {
    #![allow(unused_imports)]
    use super::*;
    use ethers::types::{Block, U256};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn test_archive_node_has_block_data() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("cannot tell the time")
            .as_secs()
            .into();

        let random_block = Block {
            hash: Some(H256::random()),
            number: Some(1_000_000.into()),
            timestamp: now,
            ..Default::default()
        };

        let random_block = Arc::new(random_block);

        let head_block = SavedBlock::new(random_block);
        let block_data_limit = u64::MAX;

        let metrics = OpenRequestHandleMetrics::default();

        let x = Web3Connection {
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
            block_data_limit: block_data_limit.into(),
            tier: 0,
            head_block: RwLock::new(Some(head_block.clone())),
            open_request_handle_metrics: Arc::new(metrics),
        };

        assert!(x.has_block_data(&0.into()));
        assert!(x.has_block_data(&1.into()));
        assert!(x.has_block_data(&head_block.number()));
        assert!(!x.has_block_data(&(head_block.number() + 1)));
        assert!(!x.has_block_data(&(head_block.number() + 1000)));
    }

    #[test]
    fn test_pruned_node_has_block_data() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("cannot tell the time")
            .as_secs()
            .into();

        let head_block: SavedBlock = Arc::new(Block {
            hash: Some(H256::random()),
            number: Some(1_000_000.into()),
            timestamp: now,
            ..Default::default()
        })
        .into();

        let block_data_limit = 64;

        let metrics = OpenRequestHandleMetrics::default();

        // TODO: this is getting long. have a `impl Default`
        let x = Web3Connection {
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
            block_data_limit: block_data_limit.into(),
            tier: 0,
            head_block: RwLock::new(Some(head_block.clone())),
            open_request_handle_metrics: Arc::new(metrics),
        };

        assert!(!x.has_block_data(&0.into()));
        assert!(!x.has_block_data(&1.into()));
        assert!(!x.has_block_data(&(head_block.number() - block_data_limit - 1)));
        assert!(x.has_block_data(&(head_block.number() - block_data_limit)));
        assert!(x.has_block_data(&head_block.number()));
        assert!(!x.has_block_data(&(head_block.number() + 1)));
        assert!(!x.has_block_data(&(head_block.number() + 1000)));
    }

    #[test]
    fn test_lagged_node_not_has_block_data() {
        let now: U256 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("cannot tell the time")
            .as_secs()
            .into();

        // head block is an hour old
        let head_block = Block {
            hash: Some(H256::random()),
            number: Some(1_000_000.into()),
            timestamp: now - 3600,
            ..Default::default()
        };

        let head_block = Arc::new(head_block);

        let head_block = SavedBlock::new(head_block);
        let block_data_limit = u64::MAX;

        let metrics = OpenRequestHandleMetrics::default();

        let x = Web3Connection {
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
            block_data_limit: block_data_limit.into(),
            tier: 0,
            head_block: RwLock::new(Some(head_block.clone())),
            open_request_handle_metrics: Arc::new(metrics),
        };

        assert!(!x.has_block_data(&0.into()));
        assert!(!x.has_block_data(&1.into()));
        assert!(!x.has_block_data(&head_block.number()));
        assert!(!x.has_block_data(&(head_block.number() + 1)));
        assert!(!x.has_block_data(&(head_block.number() + 1000)));
    }
}
