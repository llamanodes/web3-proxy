///! Rate-limited communication with a web3 provider.
use super::blockchain::{ArcBlock, BlockHashesCache, BlockId};
use super::provider::Web3Provider;
use super::request::{OpenRequestHandle, OpenRequestHandleMetrics, OpenRequestResult};
use crate::app::{flatten_handle, AnyhowJoinHandle};
use crate::config::BlockAndRpc;
use crate::frontend::authorization::AuthorizedRequest;
use anyhow::Context;
use ethers::prelude::{Block, Bytes, Middleware, ProviderError, TxHash, H256, U64};
use futures::future::try_join_all;
use futures::StreamExt;
use parking_lot::RwLock;
use rand::Rng;
use redis_rate_limiter::{RedisPool, RedisRateLimitResult, RedisRateLimiter};
use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;
use serde_json::json;
use std::cmp::min;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{self, AtomicU32, AtomicU64};
use std::{cmp::Ordering, sync::Arc};
use tokio::sync::broadcast;
use tokio::sync::RwLock as AsyncRwLock;
use tokio::time::{interval, sleep, sleep_until, Duration, Instant, MissedTickBehavior};
use tracing::{debug, error, info, instrument, trace, warn, Level};

/// An active connection to a Web3 RPC server like geth or erigon.
pub struct Web3Connection {
    pub name: String,
    /// TODO: can we get this from the provider? do we even need it?
    url: String,
    /// Some connections use an http_client. we keep a clone for reconnecting
    http_client: Option<reqwest::Client>,
    /// keep track of currently open requests. We sort on this
    pub(super) active_requests: AtomicU32,
    /// keep track of total requests
    /// TODO: is this type okay?
    /// TODO: replace this with something in metered?
    pub(super) total_requests: AtomicU64,
    /// provider is in a RwLock so that we can replace it if re-connecting
    /// it is an async lock because we hold it open across awaits
    pub(super) provider: AsyncRwLock<Option<Arc<Web3Provider>>>,
    /// rate limits are stored in a central redis so that multiple proxies can share their rate limits
    /// We do not use the deferred rate limiter because going over limits would cause errors
    hard_limit: Option<RedisRateLimiter>,
    /// used for load balancing to the least loaded server
    pub(super) soft_limit: u32,
    /// TODO: have an enum for this so that "no limit" prints pretty?
    block_data_limit: AtomicU64,
    /// Lower weight are higher priority when sending requests
    pub(super) weight: u32,
    // TODO: async lock?
    pub(super) head_block_id: RwLock<Option<BlockId>>,
    pub(super) open_request_handle_metrics: Arc<OpenRequestHandleMetrics>,
}

impl Web3Connection {
    /// Connect to a web3 rpc
    // #[instrument(name = "spawn_Web3Connection", skip(hard_limit, http_client))]
    // TODO: have this take a builder (which will have channels attached)
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn(
        name: String,
        chain_id: u64,
        url_str: String,
        // optional because this is only used for http providers. websocket providers don't use it
        http_client: Option<reqwest::Client>,
        http_interval_sender: Option<Arc<broadcast::Sender<()>>>,
        // TODO: have a builder struct for this.
        hard_limit: Option<(u64, RedisPool)>,
        // TODO: think more about this type
        soft_limit: u32,
        block_map: BlockHashesCache,
        block_sender: Option<flume::Sender<BlockAndRpc>>,
        tx_id_sender: Option<flume::Sender<(TxHash, Arc<Self>)>>,
        reconnect: bool,
        weight: u32,
        open_request_handle_metrics: Arc<OpenRequestHandleMetrics>,
    ) -> anyhow::Result<(Arc<Web3Connection>, AnyhowJoinHandle<()>)> {
        let hard_limit = hard_limit.map(|(hard_rate_limit, redis_pool)| {
            // TODO: is cache size 1 okay? i think we need
            RedisRateLimiter::new(
                "web3_proxy",
                &format!("{}:{}", chain_id, url_str),
                hard_rate_limit,
                60.0,
                redis_pool,
            )
        });

        let new_connection = Self {
            name,
            http_client,
            url: url_str.clone(),
            active_requests: 0.into(),
            total_requests: 0.into(),
            provider: AsyncRwLock::new(None),
            hard_limit,
            soft_limit,
            block_data_limit: Default::default(),
            head_block_id: RwLock::new(Default::default()),
            weight,
            open_request_handle_metrics,
        };

        let new_connection = Arc::new(new_connection);

        // connect to the server (with retries)
        new_connection
            .retrying_reconnect(block_sender.as_ref(), false)
            .await?;

        // check the server's chain_id here
        // TODO: move this outside the `new` function and into a `start` function or something. that way we can do retries from there
        // TODO: some public rpcs (on bsc and fantom) do not return an id and so this ends up being an error
        // TODO: what should the timeout be?
        let found_chain_id: Result<U64, _> = new_connection
            .wait_for_request_handle(None, Duration::from_secs(30))
            .await?
            .request(
                "eth_chainId",
                &json!(Option::None::<()>),
                Level::ERROR.into(),
            )
            .await;

        match found_chain_id {
            Ok(found_chain_id) => {
                // TODO: there has to be a cleaner way to do this
                if chain_id != found_chain_id.as_u64() {
                    return Err(anyhow::anyhow!(
                        "incorrect chain id! Config has {}, but RPC has {}",
                        chain_id,
                        found_chain_id
                    )
                    .context(format!("failed @ {}", new_connection)));
                }
            }
            Err(e) => {
                let e = anyhow::Error::from(e).context(format!("failed @ {}", new_connection));
                return Err(e);
            }
        }

        let will_subscribe_to_blocks = block_sender.is_some();

        // subscribe to new blocks and new transactions
        // TODO: make transaction subscription optional (just pass None for tx_id_sender)
        let handle = {
            let new_connection = new_connection.clone();
            tokio::spawn(async move {
                new_connection
                    .subscribe(
                        http_interval_sender,
                        block_map,
                        block_sender,
                        tx_id_sender,
                        reconnect,
                    )
                    .await
            })
        };

        // we could take "archive" as a parameter, but we would want a safety check on it regardless
        // check common archive thresholds
        // TODO: would be great if rpcs exposed this
        // TODO: move this to a helper function so we can recheck on errors or as the chain grows
        // TODO: move this to a helper function that checks
        if will_subscribe_to_blocks {
            // TODO: make sure the server isn't still syncing

            // TODO: don't sleep. wait for new heads subscription instead
            // TODO: i think instead of atomics, we could maybe use a watch channel
            sleep(Duration::from_millis(250)).await;

            new_connection.check_block_data_limit().await?;
        }

        Ok((new_connection, handle))
    }

    async fn check_block_data_limit(self: &Arc<Self>) -> anyhow::Result<Option<u64>> {
        let mut limit = None;

        for block_data_limit in [u64::MAX, 90_000, 128, 64, 32] {
            let mut head_block_id = self.head_block_id.read().clone();

            // TODO: subscribe to a channel instead of polling. subscribe to http_interval_sender?
            while head_block_id.is_none() {
                warn!(rpc=%self, "no head block yet. retrying");

                sleep(Duration::from_secs(13)).await;

                head_block_id = self.head_block_id.read().clone();
            }
            let head_block_num = head_block_id.expect("is_none was checked above").num;

            debug_assert_ne!(head_block_num, U64::zero());

            // TODO: subtract 1 from block_data_limit for safety?
            let maybe_archive_block = head_block_num
                .saturating_sub((block_data_limit).into())
                .max(U64::one());

            // TODO: wait for the handle BEFORE we check the current block number. it might be delayed too!
            // TODO: what should the request be?
            let archive_result: Result<Bytes, _> = self
                .wait_for_request_handle(None, Duration::from_secs(30))
                .await?
                .request(
                    "eth_getCode",
                    &json!((
                        "0xdead00000000000000000000000000000000beef",
                        maybe_archive_block,
                    )),
                    // error here are expected, so keep the level low
                    tracing::Level::DEBUG.into(),
                )
                .await;

            trace!(?archive_result, rpc=%self);

            if archive_result.is_ok() {
                limit = Some(block_data_limit);

                break;
            }
        }

        if let Some(limit) = limit {
            self.block_data_limit
                .store(limit, atomic::Ordering::Release);
        }

        Ok(limit)
    }

    /// TODO: this might be too simple. different nodes can prune differently. its possible we will have a block range
    pub fn block_data_limit(&self) -> U64 {
        self.block_data_limit.load(atomic::Ordering::Relaxed).into()
    }

    pub fn has_block_data(&self, needed_block_num: &U64) -> bool {
        let head_block_id = self.head_block_id.read().clone();

        let newest_block_num = match head_block_id {
            None => return false,
            Some(x) => x.num,
        };

        if needed_block_num > &newest_block_num {
            return false;
        }

        // if this is a pruning node, we might not actually have the block
        let block_data_limit: U64 = self.block_data_limit();

        let oldest_block_num = newest_block_num
            .saturating_sub(block_data_limit)
            .max(U64::one());

        needed_block_num >= &oldest_block_num
    }

    /// reconnect to the provider. errors are retried forever with exponential backoff with jitter.
    /// We use the "Decorrelated" jitter from <https://aws.amazon.com/blogs/architecture/exponential-backoff-and-jitter/>
    /// TODO: maybe it would be better to use "Full Jitter". The "Full Jitter" approach uses less work, but slightly more time.
    pub async fn retrying_reconnect(
        self: &Arc<Self>,
        block_sender: Option<&flume::Sender<BlockAndRpc>>,
        initial_sleep: bool,
    ) -> anyhow::Result<()> {
        // there are several crates that have retry helpers, but they all seem more complex than necessary
        let base_ms = 500;
        let cap_ms = 30_000;
        let range_multiplier = 3;

        // sleep once before the initial retry attempt
        // TODO: now that we use this method for our initial connection, do we still want this sleep?
        let mut sleep_ms = if initial_sleep {
            let first_sleep_ms = min(
                cap_ms,
                rand::thread_rng().gen_range(base_ms..(base_ms * range_multiplier)),
            );
            let reconnect_in = Duration::from_millis(first_sleep_ms);

            warn!(rpc=%self, ?reconnect_in, "Reconnect in");

            sleep(reconnect_in).await;

            first_sleep_ms
        } else {
            base_ms
        };

        // retry until we succeed
        while let Err(err) = self.reconnect(block_sender).await {
            sleep_ms = min(
                cap_ms,
                rand::thread_rng().gen_range(base_ms..(sleep_ms * range_multiplier)),
            );

            let retry_in = Duration::from_millis(sleep_ms);
            warn!(rpc=%self, ?retry_in, ?err, "Failed to reconnect!");

            sleep(retry_in).await;
        }

        Ok(())
    }

    /// reconnect a websocket provider
    pub async fn reconnect(
        self: &Arc<Self>,
        // websocket doesn't need the http client
        block_sender: Option<&flume::Sender<BlockAndRpc>>,
    ) -> anyhow::Result<()> {
        // since this lock is held open over an await, we use tokio's locking
        // TODO: timeout on this lock. if its slow, something is wrong
        let mut provider_option = self.provider.write().await;

        if let Some(provider) = &*provider_option {
            match provider.as_ref() {
                Web3Provider::Http(_) => {
                    // http clients don't need to do anything for reconnecting
                    // they *do* need to run this function to setup the first
                    return Ok(());
                }
                Web3Provider::Ws(_) => {}
            }

            info!(rpc=%self, "reconnecting");

            // disconnect the current provider
            *provider_option = None;

            // reset sync status
            {
                let mut head_block_id = self.head_block_id.write();
                *head_block_id = None;
            }

            // tell the block subscriber that we don't have any blocks
            if let Some(block_sender) = &block_sender {
                block_sender
                    .send_async((None, self.clone()))
                    .await
                    .context("block_sender during connect")?;
            }
        } else {
            info!(rpc=%self, "connecting");
        }

        // TODO: if this fails, keep retrying! otherwise it crashes and doesn't try again!
        let new_provider = Web3Provider::from_str(&self.url, self.http_client.clone()).await?;

        *provider_option = Some(Arc::new(new_provider));

        info!(rpc=%self, "successfully connected");

        Ok(())
    }

    #[inline]
    pub fn active_requests(&self) -> u32 {
        self.active_requests.load(atomic::Ordering::Acquire)
    }

    #[inline]
    pub async fn has_provider(&self) -> bool {
        self.provider.read().await.is_some()
    }

    async fn send_head_block_result(
        self: &Arc<Self>,
        new_head_block: Result<Option<ArcBlock>, ProviderError>,
        block_sender: &flume::Sender<BlockAndRpc>,
        block_map: BlockHashesCache,
    ) -> anyhow::Result<()> {
        match new_head_block {
            Ok(None) => {
                // TODO: i think this should clear the local block and then update over the block sender
                warn!("unsynced server {}", self);

                {
                    let mut head_block_id = self.head_block_id.write();

                    *head_block_id = None;
                }

                block_sender
                    .send_async((None, self.clone()))
                    .await
                    .context("clearing block_sender")?;
            }
            Ok(Some(new_head_block)) => {
                // TODO: is unwrap_or_default ok? we might have an empty block
                let new_hash = new_head_block.hash.unwrap_or_default();

                // if we already have this block saved, set new_head_block to that arc. otherwise store this copy
                let new_head_block = block_map
                    .get_with(new_hash, async move { new_head_block })
                    .await;

                let new_num = new_head_block.number.unwrap_or_default();

                // save the block so we don't send the same one multiple times
                // also save so that archive checks can know how far back to query
                {
                    let mut head_block_id = self.head_block_id.write();

                    if head_block_id.is_none() {
                        *head_block_id = Some(BlockId {
                            hash: new_hash,
                            num: new_num,
                        });
                    } else {
                        head_block_id.as_mut().map(|x| {
                            x.hash = new_hash;
                            x.num = new_num;
                            x
                        });
                    }
                }

                block_sender
                    .send_async((Some(new_head_block), self.clone()))
                    .await
                    .context("block_sender")?;
            }
            Err(err) => {
                warn!(?err, "unable to get block from {}", self);

                {
                    let mut head_block_id = self.head_block_id.write();

                    *head_block_id = None;
                }

                // send an empty block to take this server out of rotation
                // TODO: this is NOT working!!!!
                block_sender
                    .send_async((None, self.clone()))
                    .await
                    .context("block_sender")?;
            }
        }

        Ok(())
    }

    /// subscribe to blocks and transactions with automatic reconnects
    async fn subscribe(
        self: Arc<Self>,
        http_interval_sender: Option<Arc<broadcast::Sender<()>>>,
        block_map: BlockHashesCache,
        block_sender: Option<flume::Sender<BlockAndRpc>>,
        tx_id_sender: Option<flume::Sender<(TxHash, Arc<Self>)>>,
        reconnect: bool,
    ) -> anyhow::Result<()> {
        loop {
            debug!(rpc=%self, "subscribing");

            let http_interval_receiver = http_interval_sender.as_ref().map(|x| x.subscribe());

            let mut futures = vec![];

            if let Some(block_sender) = &block_sender {
                let f = self.clone().subscribe_new_heads(
                    http_interval_receiver,
                    block_sender.clone(),
                    block_map.clone(),
                );

                futures.push(flatten_handle(tokio::spawn(f)));
            }

            if let Some(tx_id_sender) = &tx_id_sender {
                let f = self
                    .clone()
                    .subscribe_pending_transactions(tx_id_sender.clone());

                futures.push(flatten_handle(tokio::spawn(f)));
            }

            {
                // TODO: move this into a proper function
                let conn = self.clone();
                // health check
                let f = async move {
                    loop {
                        if let Some(provider) = conn.provider.read().await.as_ref() {
                            if provider.ready() {
                                trace!(rpc=%conn, "provider is ready");
                            } else {
                                warn!(rpc=%conn, "provider is NOT ready");
                                // returning error will trigger a reconnect
                                // TODO: what if we just happened to have this check line up with another restart?
                                return Err(anyhow::anyhow!("provider is not ready"));
                            }
                        }

                        // TODO: how often?
                        // TODO: also check that the head block has changed recently
                        sleep(Duration::from_secs(10)).await;
                    }
                };

                futures.push(flatten_handle(tokio::spawn(f)));
            }

            match try_join_all(futures).await {
                Ok(_) => {
                    // futures all exited without error. break instead of restarting subscriptions
                    break;
                }
                Err(err) => {
                    if reconnect {
                        warn!(
                            rpc=%self,
                            ?err,
                            "subscription exited",
                        );

                        self.retrying_reconnect(block_sender.as_ref(), true).await?;
                    } else {
                        error!(rpc=%self, ?err, "subscription exited");
                        return Err(err);
                    }
                }
            }
        }

        info!(rpc=%self, "all subscriptions complete");

        Ok(())
    }

    /// Subscribe to new blocks. If `reconnect` is true, this runs forever.
    async fn subscribe_new_heads(
        self: Arc<Self>,
        http_interval_receiver: Option<broadcast::Receiver<()>>,
        block_sender: flume::Sender<BlockAndRpc>,
        block_map: BlockHashesCache,
    ) -> anyhow::Result<()> {
        info!(%self, "watching new heads");

        // TODO: is a RwLock of an Option<Arc> the right thing here?
        if let Some(provider) = self.provider.read().await.clone() {
            match &*provider {
                Web3Provider::Http(_provider) => {
                    // there is a "watch_blocks" function, but a lot of public nodes do not support the necessary rpc endpoints
                    // TODO: try watch_blocks and fall back to this?

                    let mut http_interval_receiver = http_interval_receiver.unwrap();

                    let mut last_hash = H256::zero();

                    loop {
                        // TODO: what should the max_wait be?
                        match self
                            .wait_for_request_handle(None, Duration::from_secs(30))
                            .await
                        {
                            Ok(active_request_handle) => {
                                let block: Result<Option<ArcBlock>, _> = active_request_handle
                                    .request(
                                        "eth_getBlockByNumber",
                                        &json!(("latest", false)),
                                        tracing::Level::ERROR.into(),
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
                                warn!(?err, "Internal error on latest block from {}", self);

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
                                    warn!(?err, rpc=%self, "http interval lagging by {}!", lagged);
                                }
                            }
                        }

                        trace!(rpc=%self, "ok http interval");
                    }
                }
                Web3Provider::Ws(provider) => {
                    // todo: move subscribe_blocks onto the request handle?
                    let active_request_handle = self
                        .wait_for_request_handle(None, Duration::from_secs(30))
                        .await;
                    let mut stream = provider.subscribe_blocks().await?;
                    drop(active_request_handle);

                    // query the block once since the subscription doesn't send the current block
                    // there is a very small race condition here where the stream could send us a new block right now
                    // all it does is print "new block" for the same block as current block
                    // TODO: how does this get wrapped in an arc? does ethers handle that?
                    let block: Result<Option<ArcBlock>, _> = self
                        .wait_for_request_handle(None, Duration::from_secs(30))
                        .await?
                        .request(
                            "eth_getBlockByNumber",
                            &json!(("latest", false)),
                            tracing::Level::ERROR.into(),
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
                    warn!(rpc=%self, "new_heads subscription ended");
                    return Err(anyhow::anyhow!("new_heads subscription ended"));
                }
            }
        }

        Ok(())
    }

    async fn subscribe_pending_transactions(
        self: Arc<Self>,
        tx_id_sender: flume::Sender<(TxHash, Arc<Self>)>,
    ) -> anyhow::Result<()> {
        info!(%self, "watching pending transactions");

        // TODO: is a RwLock of an Option<Arc> the right thing here?
        if let Some(provider) = self.provider.read().await.clone() {
            match &*provider {
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
                        .wait_for_request_handle(None, Duration::from_secs(30))
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
                    warn!(rpc=%self, "pending_transactions subscription ended");
                    return Err(anyhow::anyhow!("pending_transactions subscription ended"));
                }
            }
        }

        Ok(())
    }

    /// be careful with this; it might wait forever!
    #[instrument]
    pub async fn wait_for_request_handle(
        self: &Arc<Self>,
        authorized_request: Option<&Arc<AuthorizedRequest>>,
        max_wait: Duration,
    ) -> anyhow::Result<OpenRequestHandle> {
        let max_wait = Instant::now() + max_wait;

        loop {
            let x = self.try_request_handle(authorized_request).await;

            trace!(?x, "try_request_handle");

            match x {
                Ok(OpenRequestResult::Handle(handle)) => return Ok(handle),
                Ok(OpenRequestResult::RetryAt(retry_at)) => {
                    // TODO: emit a stat?
                    trace!(?retry_at);

                    if retry_at > max_wait {
                        // break now since we will wait past our maximum wait time
                        return Err(anyhow::anyhow!("timeout waiting for request handle"));
                    }
                    sleep_until(retry_at).await;
                }
                Ok(OpenRequestResult::RetryNever) => {
                    // TODO: when can this happen? log? emit a stat?
                    // TODO: subscribe to the head block on this
                    // TODO: sleep how long? maybe just error?
                    return Err(anyhow::anyhow!("unable to retry for request handle"));
                }
                Err(err) => return Err(err),
            }
        }
    }

    #[instrument]
    pub async fn try_request_handle(
        self: &Arc<Self>,
        authorized_request: Option<&Arc<AuthorizedRequest>>,
    ) -> anyhow::Result<OpenRequestResult> {
        // check that we are connected
        if !self.has_provider().await {
            // TODO: emit a stat?
            // TODO: wait until we have a provider?
            return Ok(OpenRequestResult::RetryNever);
        }

        // check rate limits
        if let Some(ratelimiter) = self.hard_limit.as_ref() {
            // TODO: how should we know if we should set expire or not?
            match ratelimiter.throttle().await? {
                RedisRateLimitResult::Allowed(_) => {
                    trace!("rate limit succeeded")
                }
                RedisRateLimitResult::RetryAt(retry_at, _) => {
                    // rate limit failed
                    // save the smallest retry_after. if nothing succeeds, return an Err with retry_after in it
                    // TODO: use tracing better
                    // TODO: i'm seeing "Exhausted rate limit on moralis: 0ns". How is it getting 0?
                    warn!(?retry_at, rpc=%self, "Exhausted rate limit");

                    return Ok(OpenRequestResult::RetryAt(retry_at));
                }
                RedisRateLimitResult::RetryNever => {
                    return Ok(OpenRequestResult::RetryNever);
                }
            }
        };

        let handle = OpenRequestHandle::new(self.clone(), authorized_request.cloned());

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
        let mut state = serializer.serialize_struct("Web3Connection", 6)?;

        // the url is excluded because it likely includes private information. just show the name
        state.serialize_field("name", &self.name)?;

        let block_data_limit = self.block_data_limit.load(atomic::Ordering::Relaxed);
        if block_data_limit == u64::MAX {
            state.serialize_field("block_data_limit", &None::<()>)?;
        } else {
            state.serialize_field("block_data_limit", &block_data_limit)?;
        }

        state.serialize_field("soft_limit", &self.soft_limit)?;

        state.serialize_field(
            "active_requests",
            &self.active_requests.load(atomic::Ordering::Relaxed),
        )?;

        state.serialize_field(
            "total_requests",
            &self.total_requests.load(atomic::Ordering::Relaxed),
        )?;

        let head_block_id = &*self.head_block_id.read();
        state.serialize_field("head_block_id", head_block_id)?;

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
