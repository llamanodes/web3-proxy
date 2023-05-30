///! Rate-limited communication with a web3 provider.
use super::blockchain::{ArcBlock, BlocksByHashCache, Web3ProxyBlock};
use super::provider::{connect_http, connect_ws, EthersHttpProvider, EthersWsProvider};
use super::request::{OpenRequestHandle, OpenRequestResult};
use crate::app::{flatten_handle, Web3ProxyJoinHandle};
use crate::config::{BlockAndRpc, Web3RpcConfig};
use crate::frontend::authorization::Authorization;
use crate::frontend::errors::{Web3ProxyError, Web3ProxyResult};
use crate::rpcs::request::RequestErrorHandler;
use anyhow::{anyhow, Context};
use ethers::prelude::{Bytes, Middleware, TxHash, U64};
use ethers::types::{Address, Transaction, U256};
use futures::future::try_join_all;
use futures::StreamExt;
use latency::{EwmaLatency, PeakEwmaLatency};
use log::{debug, info, trace, warn, Level};
use migration::sea_orm::DatabaseConnection;
use ordered_float::OrderedFloat;
use parking_lot::RwLock;
use redis_rate_limiter::{RedisPool, RedisRateLimitResult, RedisRateLimiter};
use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;
use serde_json::json;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{self, AtomicU64, AtomicUsize};
use std::{cmp::Ordering, sync::Arc};
use tokio::sync::watch;
use tokio::time::{sleep, sleep_until, timeout, Duration, Instant};
use url::Url;

/// An active connection to a Web3 RPC server like geth or erigon.
#[derive(Default)]
pub struct Web3Rpc {
    pub name: String,
    pub display_name: Option<String>,
    pub db_conn: Option<DatabaseConnection>,
    /// most all requests prefer use the http_provider
    pub(super) http_provider: Option<EthersHttpProvider>,
    /// the websocket provider is only used for subscriptions
    pub(super) ws_provider: Option<EthersWsProvider>,
    /// keep track of hard limits
    /// this is only inside an Option so that the "Default" derive works. it will always be set.
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
    /// Lower tiers are higher priority when sending requests
    pub(super) tier: u64,
    /// TODO: change this to a watch channel so that http providers can subscribe and take action on change.
    /// this is only inside an Option so that the "Default" derive works. it will always be set.
    pub(super) head_block: Option<watch::Sender<Option<Web3ProxyBlock>>>,
    /// Track head block latency
    pub(super) head_latency: RwLock<EwmaLatency>,
    /// Track peak request latency
    /// This is only inside an Option so that the "Default" derive works. it will always be set.
    pub(super) peak_latency: Option<PeakEwmaLatency>,
    /// Track total requests served
    /// TODO: maybe move this to graphana
    pub(super) total_requests: AtomicUsize,
    pub(super) active_requests: AtomicUsize,
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
        redis_pool: Option<RedisPool>,
        block_interval: Duration,
        block_map: BlocksByHashCache,
        block_sender: Option<flume::Sender<BlockAndRpc>>,
        tx_id_sender: Option<flume::Sender<(TxHash, Arc<Self>)>>,
    ) -> anyhow::Result<(Arc<Web3Rpc>, Web3ProxyJoinHandle<()>)> {
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

        // have a sender for tracking hard limit anywhere. we use this in case we
        // and track on servers that have a configured hard limit
        let (hard_limit_until, _) = watch::channel(Instant::now());

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

        let http_provider = if let Some(http_url) = config.http_url {
            let http_url = http_url.parse::<Url>()?;

            Some(connect_http(http_url, http_client, block_interval)?)

            // TODO: check the provider is on the right chain
        } else {
            None
        };

        let ws_provider = if let Some(ws_url) = config.ws_url {
            let ws_url = ws_url.parse::<Url>()?;

            Some(connect_ws(ws_url, usize::MAX).await?)

            // TODO: check the provider is on the right chain
        } else {
            None
        };

        let (disconnect_watch, _) = watch::channel(false);

        let new_rpc = Self {
            automatic_block_limit,
            backup,
            block_data_limit,
            created_at: Some(created_at),
            db_conn: db_conn.clone(),
            display_name: config.display_name,
            hard_limit,
            hard_limit_until: Some(hard_limit_until),
            head_block: Some(head_block),
            http_provider,
            name,
            peak_latency: Some(peak_latency),
            soft_limit: config.soft_limit,
            tier: config.tier,
            ws_provider,
            disconnect_watch: Some(disconnect_watch),
            ..Default::default()
        };

        let new_connection = Arc::new(new_rpc);

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
                        tx_id_sender,
                    )
                    .await
            })
        };

        Ok((new_connection, handle))
    }

    pub fn peak_ewma(&self) -> OrderedFloat<f64> {
        let peak_latency = if let Some(peak_latency) = self.peak_latency.as_ref() {
            peak_latency.latency().as_secs_f64()
        } else {
            1.0
        };

        // TODO: what ordering?
        let active_requests = self.active_requests.load(atomic::Ordering::Acquire) as f64 + 1.0;

        OrderedFloat(peak_latency * active_requests)
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

        // TODO: check eth_syncing. if it is not false, return Ok(None)

        let mut limit = None;

        // TODO: binary search between 90k and max?
        // TODO: start at 0 or 1?
        for block_data_limit in [0, 32, 64, 128, 256, 512, 1024, 90_000, u64::MAX] {
            let head_block_num_future = self.request::<Option<()>, U256>(
                "eth_blockNumber",
                &None,
                // error here are expected, so keep the level low
                Level::Debug.into(),
                authorization.clone(),
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
            let archive_result: Result<Bytes, _> = self
                .request(
                    "eth_getCode",
                    &json!((
                        "0xdead00000000000000000000000000000000beef",
                        maybe_archive_block,
                    )),
                    // error here are expected, so keep the level low
                    Level::Trace.into(),
                    authorization.clone(),
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

    /// TODO: get rid of this now that consensus rpcs does it
    pub fn has_block_data(&self, needed_block_num: &U64) -> bool {
        let head_block_num = match self.head_block.as_ref().unwrap().borrow().as_ref() {
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
    async fn check_provider(self: &Arc<Self>, chain_id: u64) -> Web3ProxyResult<()> {
        let authorization = Arc::new(Authorization::internal(self.db_conn.clone())?);

        // check the server's chain_id here
        // TODO: some public rpcs (on bsc and fantom) do not return an id and so this ends up being an error
        // TODO: what should the timeout be? should there be a request timeout?
        // trace!("waiting on chain id for {}", self);
        let found_chain_id: Result<U64, _> = self
            .request(
                "eth_chainId",
                &json!(Vec::<()>::new()),
                Level::Trace.into(),
                authorization.clone(),
            )
            .await;
        trace!("found_chain_id: {:#?}", found_chain_id);

        match found_chain_id {
            Ok(found_chain_id) => {
                // TODO: there has to be a cleaner way to do this
                if chain_id != found_chain_id.as_u64() {
                    return Err(anyhow::anyhow!(
                        "incorrect chain id! Config has {}, but RPC has {}",
                        chain_id,
                        found_chain_id
                    )
                    .context(format!("failed @ {}", self))
                    .into());
                }
            }
            Err(e) => {
                return Err(anyhow::Error::from(e)
                    .context(format!("unable to parse eth_chainId from {}", self))
                    .into());
            }
        }

        self.check_block_data_limit(&authorization)
            .await
            .context(format!("unable to check_block_data_limit of {}", self))?;

        info!("successfully connected to {}", self);

        Ok(())
    }

    pub(crate) async fn send_head_block_result(
        self: &Arc<Self>,
        new_head_block: Web3ProxyResult<Option<ArcBlock>>,
        block_sender: &flume::Sender<BlockAndRpc>,
        block_map: &BlocksByHashCache,
    ) -> Web3ProxyResult<()> {
        let new_head_block = match new_head_block {
            Ok(None) => {
                let head_block_tx = self.head_block.as_ref().unwrap();

                if head_block_tx.borrow().is_none() {
                    // we previously sent a None. return early
                    return Ok(());
                }

                let age = self.created_at.unwrap().elapsed().as_millis();

                debug!("clearing head block on {} ({}ms old)!", self, age);

                head_block_tx.send_replace(None);

                None
            }
            Ok(Some(new_head_block)) => {
                let new_head_block = Web3ProxyBlock::try_new(new_head_block)
                    .expect("blocks from newHeads subscriptions should also convert");

                let new_hash = *new_head_block.hash();

                // if we already have this block saved, set new_head_block to that arc. otherwise store this copy
                let new_head_block = block_map
                    .get_or_insert_async(&new_hash, async move { new_head_block })
                    .await;

                // save the block so we don't send the same one multiple times
                // also save so that archive checks can know how far back to query
                self.head_block
                    .as_ref()
                    .unwrap()
                    .send_replace(Some(new_head_block.clone()));

                if self.block_data_limit() == U64::zero() {
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

                self.head_block.as_ref().unwrap().send_replace(None);

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

    async fn healthcheck(
        self: &Arc<Self>,
        authorization: &Arc<Authorization>,
        error_handler: RequestErrorHandler,
    ) -> Web3ProxyResult<()> {
        let head_block = self.head_block.as_ref().unwrap().borrow().clone();

        if let Some(head_block) = head_block {
            let head_block = head_block.block;

            // TODO: if head block is very old and not expected to be syncing, emit warning

            let block_number = head_block.number.context("no block number")?;

            let to = if let Some(txid) = head_block.transactions.last().cloned() {
                let tx = self
                    .request::<_, Option<Transaction>>(
                        "eth_getTransactionByHash",
                        &(txid,),
                        error_handler,
                        authorization.clone(),
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
                .request::<_, Option<Bytes>>(
                    "eth_getCode",
                    &(to, block_number),
                    error_handler,
                    authorization.clone(),
                )
                .await?;
        } else {
            // TODO: if head block is none for too long, give an error
        }

        Ok(())
    }

    /// subscribe to blocks and transactions
    /// This should only exit when the program is exiting.
    /// TODO: should more of these args be on self? chain_id for sure
    #[allow(clippy::too_many_arguments)]
    async fn subscribe(
        self: Arc<Self>,
        authorization: &Arc<Authorization>,
        block_map: BlocksByHashCache,
        block_sender: Option<flume::Sender<BlockAndRpc>>,
        chain_id: u64,
        tx_id_sender: Option<flume::Sender<(TxHash, Arc<Self>)>>,
    ) -> Web3ProxyResult<()> {
        let error_handler = if self.backup {
            RequestErrorHandler::DebugLevel
        } else {
            RequestErrorHandler::ErrorLevel
        };

        debug!("starting subscriptions on {}", self);

        self.check_provider(chain_id).await?;

        let mut futures = vec![];

        // health check that runs if there haven't been any recent requests
        {
            // TODO: move this into a proper function
            let authorization = authorization.clone();
            let rpc = self.clone();

            // TODO: how often? different depending on the chain?
            // TODO: reset this timeout when a new block is seen? we need to keep request_latency updated though
            let health_sleep_seconds = 10;

            // health check loop
            let f = async move {
                // TODO: benchmark this and lock contention
                let mut old_total_requests = 0;
                let mut new_total_requests;

                // TODO: errors here should not cause the loop to exit!
                while !rpc.should_disconnect() {
                    new_total_requests = rpc.total_requests.load(atomic::Ordering::Relaxed);

                    if new_total_requests - old_total_requests < 10 {
                        // TODO: if this fails too many times, reset the connection
                        // TODO: move this into a function and the chaining should be easier
                        if let Err(err) = rpc.healthcheck(&authorization, error_handler).await {
                            // TODO: different level depending on the error handler
                            warn!("health checking {} failed: {:?}", rpc, err);
                        }
                    }

                    // TODO: should we count the requests done inside this health check
                    old_total_requests = new_total_requests;

                    sleep(Duration::from_secs(health_sleep_seconds)).await;
                }

                debug!("healthcheck loop on {} exited", rpc);

                Ok(())
            };

            futures.push(flatten_handle(tokio::spawn(f)));
        }

        // subscribe to new heads
        if let Some(block_sender) = &block_sender {
            // TODO: do we need this to be abortable?
            let f = self.clone().subscribe_new_heads(
                authorization.clone(),
                block_sender.clone(),
                block_map.clone(),
            );

            futures.push(flatten_handle(tokio::spawn(f)));
        }

        // subscribe pending transactions
        // TODO: make this opt-in. its a lot of bandwidth
        if let Some(tx_id_sender) = tx_id_sender {
            // TODO: do we need this to be abortable?
            let f = self
                .clone()
                .subscribe_pending_transactions(authorization.clone(), tx_id_sender);

            futures.push(flatten_handle(tokio::spawn(f)));
        }

        // try_join on the futures
        if let Err(err) = try_join_all(futures).await {
            warn!("subscription erred: {:?}", err);
        }

        debug!("subscriptions on {} exited", self);

        self.disconnect_watch
            .as_ref()
            .expect("disconnect_watch should always be set")
            .send_replace(true);

        Ok(())
    }

    /// Subscribe to new blocks.
    async fn subscribe_new_heads(
        self: Arc<Self>,
        authorization: Arc<Authorization>,
        block_sender: flume::Sender<BlockAndRpc>,
        block_map: BlocksByHashCache,
    ) -> Web3ProxyResult<()> {
        debug!("subscribing to new heads on {}", self);

        if let Some(ws_provider) = self.ws_provider.as_ref() {
            // todo: move subscribe_blocks onto the request handle
            let active_request_handle = self.wait_for_request_handle(&authorization, None).await;
            let mut blocks = ws_provider.subscribe_blocks().await?;
            drop(active_request_handle);

            // query the block once since the subscription doesn't send the current block
            // there is a very small race condition here where the stream could send us a new block right now
            // but all seeing the same block twice won't break anything
            // TODO: how does this get wrapped in an arc? does ethers handle that?
            // TODO: can we force this to use the websocket?
            let latest_block: Result<Option<ArcBlock>, _> = self
                .request(
                    "eth_getBlockByNumber",
                    &json!(("latest", false)),
                    Level::Warn.into(),
                    authorization,
                )
                .await;

            self.send_head_block_result(latest_block, &block_sender, &block_map)
                .await?;

            while let Some(block) = blocks.next().await {
                if self.should_disconnect() {
                    break;
                }

                let block = Arc::new(block);

                self.send_head_block_result(Ok(Some(block)), &block_sender, &block_map)
                    .await?;
            }
        } else if let Some(http_provider) = self.http_provider.as_ref() {
            // there is a "watch_blocks" function, but a lot of public nodes do not support the necessary rpc endpoints
            let mut blocks = http_provider.watch_blocks().await?;

            while let Some(block_hash) = blocks.next().await {
                if self.should_disconnect() {
                    break;
                }

                let block = if let Some(block) = block_map.get(&block_hash) {
                    block.block
                } else if let Some(block) = http_provider.get_block(block_hash).await? {
                    Arc::new(block)
                } else {
                    continue;
                };

                self.send_head_block_result(Ok(Some(block)), &block_sender, &block_map)
                    .await?;
            }
        } else {
            unimplemented!("no ws or http provider!")
        }

        // clear the head block. this might not be needed, but it won't hurt
        self.send_head_block_result(Ok(None), &block_sender, &block_map)
            .await?;

        if self.should_disconnect() {
            Ok(())
        } else {
            Err(anyhow!("new_heads subscription exited. reconnect needed").into())
        }
    }

    /// Turn on the firehose of pending transactions
    async fn subscribe_pending_transactions(
        self: Arc<Self>,
        authorization: Arc<Authorization>,
        tx_id_sender: flume::Sender<(TxHash, Arc<Self>)>,
    ) -> Web3ProxyResult<()> {
        // TODO: make this subscription optional
        self.wait_for_disconnect().await?;

        /*
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
        */

        if self.should_disconnect() {
            Ok(())
        } else {
            Err(anyhow!("pending_transactions subscription exited. reconnect needed").into())
        }
    }

    /// be careful with this; it might wait forever!
    pub async fn wait_for_request_handle<'a>(
        self: &'a Arc<Self>,
        authorization: &'a Arc<Authorization>,
        max_wait: Option<Duration>,
    ) -> Web3ProxyResult<OpenRequestHandle> {
        let max_wait = max_wait.map(|x| Instant::now() + x);

        loop {
            match self.try_request_handle(authorization).await {
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
                    // TODO: instead of an arbitrary sleep, subscribe to the head block on this?
                    sleep(Duration::from_millis(10)).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    pub async fn try_request_handle(
        self: &Arc<Self>,
        authorization: &Arc<Authorization>,
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
                    warn!("how did retry never on {} happen?", self);
                    return Ok(OpenRequestResult::NotReady);
                }
            }
        };

        let handle = OpenRequestHandle::new(authorization.clone(), self.clone()).await;

        Ok(OpenRequestResult::Handle(handle))
    }

    async fn wait_for_disconnect(&self) -> Result<(), tokio::sync::watch::error::RecvError> {
        let mut disconnect_subscription = self.disconnect_watch.as_ref().unwrap().subscribe();

        loop {
            if *disconnect_subscription.borrow_and_update() {
                // disconnect watch is set to "true"
                return Ok(());
            }

            // wait for disconnect_subscription to change
            disconnect_subscription.changed().await?;
        }
    }

    pub async fn request<P, R>(
        self: &Arc<Self>,
        method: &str,
        params: &P,
        revert_handler: RequestErrorHandler,
        authorization: Arc<Authorization>,
    ) -> Web3ProxyResult<R>
    where
        // TODO: not sure about this type. would be better to not need clones, but measure and spawns combine to need it
        P: Clone + fmt::Debug + serde::Serialize + Send + Sync + 'static,
        R: serde::Serialize + serde::de::DeserializeOwned + fmt::Debug + Send,
    {
        // TODO: take max_wait as a function argument?
        let x = self
            .wait_for_request_handle(&authorization, None)
            .await?
            .request::<P, R>(method, params, revert_handler)
            .await?;

        Ok(x)
    }
}

impl Hash for Web3Rpc {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.display_name.hash(state);
        self.http_provider.as_ref().map(|x| x.url()).hash(state);
        // TODO: figure out how to get the url for the provider
        // TODO: url does NOT include the authorization data. i think created_at should protect us if auth changes without anything else
        // self.ws_provider.map(|x| x.url()).hash(state);
        self.automatic_block_limit.hash(state);
        self.backup.hash(state);
        // TODO: don't include soft_limit if we change them to be dynamic
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
        let mut state = serializer.serialize_struct("Web3Rpc", 12)?;

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
            let head_block = self.head_block.as_ref().unwrap();
            let head_block = head_block.borrow();
            let head_block = head_block.as_ref();
            state.serialize_field("head_block", &head_block)?;
        }

        state.serialize_field(
            "total_requests",
            &self.total_requests.load(atomic::Ordering::Acquire),
        )?;

        state.serialize_field(
            "active_requests",
            &self.active_requests.load(atomic::Ordering::Relaxed),
        )?;

        state.serialize_field("head_latency_ms", &self.head_latency.read().value())?;

        state.serialize_field(
            "peak_latency_ms",
            &self.peak_latency.as_ref().unwrap().latency().as_millis(),
        )?;

        state.serialize_field("peak_ewma_s", self.peak_ewma().as_ref())?;

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
            tier: 0,
            head_block: Some(tx),
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
            tier: 0,
            head_block: Some(tx),
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
