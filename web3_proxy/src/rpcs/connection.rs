///! Rate-limited communication with a web3 provider.
use super::blockchain::{ArcBlock, BlockHashesMap, BlockId};
use super::provider::Web3Provider;
use super::request::{OpenRequestHandle, OpenRequestResult};
use crate::app::{flatten_handle, AnyhowJoinHandle};
use crate::config::BlockAndRpc;
use anyhow::Context;
use ethers::prelude::{Block, Bytes, Middleware, ProviderError, TxHash, H256, U64};
use futures::future::try_join_all;
use futures::StreamExt;
use parking_lot::RwLock;
use redis_rate_limit::{RedisPool, RedisRateLimit, ThrottleResult};
use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{self, AtomicU32, AtomicU64};
use std::{cmp::Ordering, sync::Arc};
use tokio::sync::broadcast;
use tokio::sync::RwLock as AsyncRwLock;
use tokio::time::{interval, sleep, sleep_until, Duration, MissedTickBehavior};
use tracing::{debug, error, info, instrument, trace, warn};

/// An active connection to a Web3Rpc
pub struct Web3Connection {
    pub name: String,
    /// TODO: can we get this from the provider? do we even need it?
    url: String,
    /// keep track of currently open requests. We sort on this
    pub(super) active_requests: AtomicU32,
    /// keep track of total requests
    /// TODO: is this type okay?
    pub(super) total_requests: AtomicU64,
    /// provider is in a RwLock so that we can replace it if re-connecting
    /// it is an async lock because we hold it open across awaits
    pub(super) provider: AsyncRwLock<Option<Arc<Web3Provider>>>,
    /// rate limits are stored in a central redis so that multiple proxies can share their rate limits
    hard_limit: Option<RedisRateLimit>,
    /// used for load balancing to the least loaded server
    pub(super) soft_limit: u32,
    /// TODO: have an enum for this so that "no limit" prints pretty?
    block_data_limit: AtomicU64,
    /// Lower weight are higher priority when sending requests
    pub(super) weight: u32,
    // TODO: async lock?
    pub(super) head_block_id: RwLock<Option<BlockId>>,
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
        block_map: BlockHashesMap,
        block_sender: Option<flume::Sender<BlockAndRpc>>,
        tx_id_sender: Option<flume::Sender<(TxHash, Arc<Self>)>>,
        reconnect: bool,
        weight: u32,
    ) -> anyhow::Result<(Arc<Web3Connection>, AnyhowJoinHandle<()>)> {
        let hard_limit = hard_limit.map(|(hard_rate_limit, redis_conection)| {
            // TODO: allow configurable period and max_burst
            RedisRateLimit::new(
                redis_conection,
                "web3_proxy",
                &format!("{}:{}", chain_id, url_str),
                hard_rate_limit,
                60.0,
            )
        });

        let provider = Web3Provider::from_str(&url_str, http_client).await?;

        let new_connection = Self {
            name,
            url: url_str.clone(),
            active_requests: 0.into(),
            total_requests: 0.into(),
            provider: AsyncRwLock::new(Some(Arc::new(provider))),
            hard_limit,
            soft_limit,
            block_data_limit: Default::default(),
            head_block_id: RwLock::new(Default::default()),
            weight,
        };

        let new_connection = Arc::new(new_connection);

        // check the server's chain_id here
        // TODO: move this outside the `new` function and into a `start` function or something. that way we can do retries from there
        // TODO: some public rpcs (on bsc and fantom) do not return an id and so this ends up being an error
        // TODO: this will wait forever. do we want that?
        let found_chain_id: Result<U64, _> = new_connection
            .wait_for_request_handle()
            .await?
            .request("eth_chainId", Option::None::<()>)
            .await;

        match found_chain_id {
            Ok(found_chain_id) => {
                // TODO: there has to be a cleaner way to do this
                if chain_id != found_chain_id.as_u64() {
                    return Err(anyhow::anyhow!(
                        "incorrect chain id! Expected {}. Found {}",
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

    #[instrument(skip_all)]
    async fn check_block_data_limit(self: &Arc<Self>) -> anyhow::Result<Option<u64>> {
        let mut limit = None;

        for block_data_limit in [u64::MAX, 90_000, 128, 64, 32] {
            let mut head_block_id = self.head_block_id.read().clone();

            // TODO: wait until head block is set outside the loop? if we disconnect while starting we could actually get 0 though
            while head_block_id.is_none() {
                warn!(rpc=%self, "no head block yet. retrying");

                // TODO: subscribe to a channel instead of polling? subscribe to http_interval_sender?
                sleep(Duration::from_secs(1)).await;

                head_block_id = self.head_block_id.read().clone();
            }
            let head_block_num = head_block_id.expect("is_none was checked above").num;

            debug_assert_ne!(head_block_num, U64::zero());

            // TODO: subtract 1 from block_data_limit for safety?
            let maybe_archive_block = head_block_num
                .saturating_sub((block_data_limit).into())
                .max(U64::one());

            // TODO: wait for the handle BEFORE we check the current block number. it might be delayed too!
            let archive_result: Result<Bytes, _> = self
                .wait_for_request_handle()
                .await?
                .request(
                    "eth_getCode",
                    (
                        "0xdead00000000000000000000000000000000beef",
                        maybe_archive_block,
                    ),
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

    /// TODO: this might be too simple. different nodes can prune differently
    pub fn block_data_limit(&self) -> U64 {
        self.block_data_limit.load(atomic::Ordering::Acquire).into()
    }

    pub fn has_block_data(&self, needed_block_num: &U64) -> bool {
        let block_data_limit: U64 = self.block_data_limit();

        let head_block_id = self.head_block_id.read().clone();

        let newest_block_num = match head_block_id {
            None => return false,
            Some(x) => x.num,
        };

        let oldest_block_num = newest_block_num
            .saturating_sub(block_data_limit)
            .max(U64::one());

        needed_block_num >= &oldest_block_num && needed_block_num <= &newest_block_num
    }

    /// reconnect a websocket provider
    #[instrument(skip_all)]
    pub async fn reconnect(
        self: &Arc<Self>,
        block_sender: Option<flume::Sender<BlockAndRpc>>,
    ) -> anyhow::Result<()> {
        // TODO: no-op if this called on a http provider
        // websocket doesn't need the http client
        let http_client = None;

        info!(rpc=%self, "reconnecting");

        // since this lock is held open over an await, we use tokio's locking
        // TODO: timeout on this lock. if its slow, something is wrong
        {
            let mut provider = self.provider.write().await;

            *provider = None;

            // TODO: if this fails, keep retrying
            let new_provider = Web3Provider::from_str(&self.url, http_client)
                .await
                .unwrap();

            *provider = Some(Arc::new(new_provider));
        }

        // tell the block subscriber that we don't have any blocks
        if let Some(block_sender) = block_sender {
            block_sender
                .send_async((None, self.clone()))
                .await
                .context("block_sender during reconnect")?;
        }

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

    #[instrument(skip_all)]
    async fn send_head_block_result(
        self: &Arc<Self>,
        new_head_block: Result<Option<ArcBlock>, ProviderError>,
        block_sender: &flume::Sender<BlockAndRpc>,
        block_map: BlockHashesMap,
    ) -> anyhow::Result<()> {
        match new_head_block {
            Ok(None) => {
                todo!("handle no block")
            }
            Ok(Some(mut new_head_block)) => {
                // TODO: is unwrap_or_default ok? we might have an empty block
                let new_hash = new_head_block.hash.unwrap_or_default();

                // if we already have this block saved, set new_block_head to that arc and don't store this copy
                // TODO: small race here
                new_head_block = if let Some(existing_block) = block_map.get(&new_hash) {
                    // we only save blocks with a total difficulty
                    debug_assert!(existing_block.total_difficulty.is_some());
                    existing_block
                } else if new_head_block.total_difficulty.is_some() {
                    // this block has a total difficulty, it is safe to use
                    block_map.insert(new_hash, new_head_block).await;

                    // we get instead of return new_head_block just in case there was a race
                    // TODO: but how bad is this race? it might be fine
                    block_map.get(&new_hash).expect("we just inserted")
                } else {
                    // Cache miss and NO TOTAL DIFFICULTY!
                    // self got the head block first. unfortunately its missing a necessary field
                    // keep this even after https://github.com/ledgerwatch/erigon/issues/5190 is closed.
                    // there are other clients and we might have to use a third party without the td fix.
                    trace!(rpc=%self, ?new_hash, "total_difficulty missing");
                    // todo: this can wait forever!
                    let complete_head_block: Block<TxHash> = self
                        .wait_for_request_handle()
                        .await?
                        .request("eth_getBlockByHash", (new_hash, false))
                        .await?;

                    debug_assert!(complete_head_block.total_difficulty.is_some());

                    block_map
                        .insert(new_hash, Arc::new(complete_head_block))
                        .await;

                    // we get instead of return new_head_block just in case there was a race
                    // TODO: but how bad is this race? it might be fine
                    block_map.get(&new_hash).expect("we just inserted")
                };

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
            Err(e) => {
                warn!("unable to get block from {}: {}", self, e);
                // TODO: do something to rpc_chain?

                // send an empty block to take this server out of rotation
                block_sender
                    .send_async((None, self.clone()))
                    .await
                    .context("block_sender")?;
            }
        }

        Ok(())
    }

    #[instrument(skip_all)]
    async fn subscribe(
        self: Arc<Self>,
        http_interval_sender: Option<Arc<broadcast::Sender<()>>>,
        block_map: BlockHashesMap,
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
                                return Err(anyhow::anyhow!("provider is not ready"));
                            }
                        }

                        // TODO: how often?
                        // TODO: should we also check that the head block has changed recently?
                        // TODO: maybe instead we should do a simple subscription and follow that instead
                        sleep(Duration::from_secs(10)).await;
                    }
                };

                futures.push(flatten_handle(tokio::spawn(f)));
            }

            match try_join_all(futures).await {
                Ok(_) => break,
                Err(err) => {
                    if reconnect {
                        // TODO: exponential backoff
                        let retry_in = Duration::from_secs(1);
                        warn!(
                            rpc=%self,
                            ?err,
                            ?retry_in,
                            "subscription exited",
                        );
                        sleep(retry_in).await;

                        // TODO: loop on reconnecting! do not return with a "?" here
                        self.reconnect(block_sender.clone()).await?;
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
    #[instrument(skip_all)]
    async fn subscribe_new_heads(
        self: Arc<Self>,
        http_interval_receiver: Option<broadcast::Receiver<()>>,
        block_sender: flume::Sender<BlockAndRpc>,
        block_map: BlockHashesMap,
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
                        // TODO: try, or wait_for?
                        match self.wait_for_request_handle().await {
                            Ok(active_request_handle) => {
                                let block: Result<Block<TxHash>, _> = active_request_handle
                                    .request("eth_getBlockByNumber", ("latest", false))
                                    .await;

                                match block {
                                    Ok(block) => {
                                        // don't send repeat blocks
                                        let new_hash = block
                                            .hash
                                            .expect("blocks here should always have hashes");

                                        if new_hash != last_hash {
                                            // new hash!
                                            last_hash = new_hash;

                                            self.send_head_block_result(
                                                Ok(Some(Arc::new(block))),
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
                    let active_request_handle = self.wait_for_request_handle().await;
                    let mut stream = provider.subscribe_blocks().await?;
                    drop(active_request_handle);

                    // query the block once since the subscription doesn't send the current block
                    // there is a very small race condition here where the stream could send us a new block right now
                    // all it does is print "new block" for the same block as current block
                    let block: Result<Option<ArcBlock>, _> = self
                        .wait_for_request_handle()
                        .await?
                        .request("eth_getBlockByNumber", ("latest", false))
                        .await
                        .map(|x| Some(Arc::new(x)));

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

                    // TODO: is this always an error?
                    // TODO: we probably don't want a warn and to return error
                    warn!(rpc=%self, "new_heads subscription ended");
                    return Err(anyhow::anyhow!("new_heads subscription ended"));
                }
            }
        }

        Ok(())
    }

    #[instrument(skip_all)]
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
                    let active_request_handle = self.wait_for_request_handle().await;

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
    // TODO: maximum wait time?
    #[instrument]
    pub async fn wait_for_request_handle(self: &Arc<Self>) -> anyhow::Result<OpenRequestHandle> {
        // TODO: maximum wait time? i think timeouts in other parts of the code are probably best

        loop {
            let x = self.try_request_handle().await;

            trace!(?x, "try_request_handle");

            match x {
                Ok(OpenRequestResult::Handle(handle)) => return Ok(handle),
                Ok(OpenRequestResult::RetryAt(retry_at)) => {
                    // TODO: emit a stat?
                    trace!(?retry_at);
                    sleep_until(retry_at).await;
                }
                Ok(OpenRequestResult::RetryNever) => {
                    // TODO: when can this happen? log? emit a stat?
                    // TODO: subscribe to the head block on this
                    // TODO: sleep how long? maybe just error?
                    return Err(anyhow::anyhow!("unable to retry"));
                }
                Err(err) => return Err(err),
            }
        }
    }

    #[instrument]
    pub async fn try_request_handle(self: &Arc<Self>) -> anyhow::Result<OpenRequestResult> {
        // check that we are connected
        if !self.has_provider().await {
            // TODO: emit a stat?
            return Ok(OpenRequestResult::RetryNever);
        }

        // check rate limits
        if let Some(ratelimiter) = self.hard_limit.as_ref() {
            match ratelimiter.throttle().await {
                Ok(ThrottleResult::Allowed) => {
                    trace!("rate limit succeeded")
                }
                Ok(ThrottleResult::RetryAt(retry_at)) => {
                    // rate limit failed
                    // save the smallest retry_after. if nothing succeeds, return an Err with retry_after in it
                    // TODO: use tracing better
                    // TODO: i'm seeing "Exhausted rate limit on moralis: 0ns". How is it getting 0?
                    warn!(?retry_at, rpc=%self, "Exhausted rate limit");

                    return Ok(OpenRequestResult::RetryAt(retry_at.into()));
                }
                Ok(ThrottleResult::RetryNever) => {
                    return Err(anyhow::anyhow!("Rate limit failed."));
                }
                Err(err) => {
                    return Err(err);
                }
            }
        };

        let handle = OpenRequestHandle::new(self.clone());

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
