use super::{AppStat, FlushedStats, RpcQueryKey};
use crate::app::Web3ProxyJoinHandle;
use crate::caches::{RpcSecretKeyCache, UserBalanceCache};
use crate::errors::Web3ProxyResult;
use crate::frontend::authorization::{AuthorizationType, Web3Request};
use crate::globals::global_db_conn;
use crate::stats::RpcQueryStats;
use derive_more::From;
use futures::stream;
use hashbrown::HashMap;
use migration::sea_orm::prelude::Decimal;
use std::time::Duration;
use tokio::select;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::{interval, sleep};
use tracing::{debug, error, info, trace, warn, Instrument};

#[derive(Debug, Default)]
pub struct BufferedRpcQueryStats {
    pub frontend_requests: u64,
    pub backend_requests: u64,
    pub backend_retries: u64,
    pub no_servers: u64,
    pub cache_misses: u64,
    pub cache_hits: u64,
    pub sum_request_bytes: u64,
    pub sum_response_bytes: u64,
    pub sum_response_millis: u64,
    pub sum_credits_used: Decimal,
    pub sum_cu_used: Decimal,
    pub paid_credits_used: Decimal,
    /// The user's balance at this point in time.
    /// Multiple queries might be modifying it at once, so this is a copy of it when received
    /// None if this is an unauthenticated request
    pub approximate_balance_remaining: Option<Decimal>,
}

#[derive(From)]
pub struct SpawnedStatBuffer {
    pub stat_sender: mpsc::UnboundedSender<AppStat>,
    /// these handles are important and must be allowed to finish
    pub background_handle: Web3ProxyJoinHandle<()>,
}

pub struct StatBuffer {
    accounting_db_buffer: HashMap<RpcQueryKey, BufferedRpcQueryStats>,
    billing_period_seconds: i64,
    chain_id: u64,
    db_save_interval_seconds: u32,
    global_timeseries_buffer: HashMap<RpcQueryKey, BufferedRpcQueryStats>,
    influxdb_bucket: Option<String>,
    influxdb_client: Option<influxdb2::Client>,
    /// a globally unique integer. max of 1e6-1
    /// instance names can be re-used but they MUST only ever be used by a single server at a time!
    /// this will be combined with tsdb_window to create a number with a max of 1e9-1
    uniq_id: i64,
    opt_in_timeseries_buffer: HashMap<RpcQueryKey, BufferedRpcQueryStats>,
    rpc_secret_key_cache: RpcSecretKeyCache,
    tsdb_save_interval_seconds: u32,
    /// a wrapping counter to keep stats from old times that got delayed from being seen as a duplicate
    tsdb_window: i64,
    num_tsdb_windows: i64,
    user_balance_cache: UserBalanceCache,

    _flush_sender: mpsc::Sender<oneshot::Sender<FlushedStats>>,
}

impl StatBuffer {
    #[allow(clippy::too_many_arguments)]
    pub fn try_spawn(
        billing_period_seconds: i64,
        chain_id: u64,
        db_save_interval_seconds: u32,
        influxdb_bucket: Option<String>,
        mut influxdb_client: Option<influxdb2::Client>,
        rpc_secret_key_cache: RpcSecretKeyCache,
        user_balance_cache: UserBalanceCache,
        shutdown_receiver: broadcast::Receiver<()>,
        tsdb_save_interval_seconds: u32,
        flush_sender: mpsc::Sender<oneshot::Sender<FlushedStats>>,
        flush_receiver: mpsc::Receiver<oneshot::Sender<FlushedStats>>,
        uniq_id: i64,
    ) -> anyhow::Result<Option<SpawnedStatBuffer>> {
        if influxdb_bucket.is_none() {
            influxdb_client = None;
        }

        let (stat_sender, stat_receiver) = mpsc::unbounded_channel();

        let num_tsdb_windows = 1_000;

        // start tsdb_window at num_tsdb_windows because the first run increments it at the start and wraps it back to 0
        let tsdb_window = num_tsdb_windows;

        let uniq_id = uniq_id * num_tsdb_windows;

        assert!(uniq_id < 1_000_000_000, "uniq_id too large!");

        let mut new = Self {
            accounting_db_buffer: Default::default(),
            billing_period_seconds,
            chain_id,
            db_save_interval_seconds,
            global_timeseries_buffer: Default::default(),
            influxdb_bucket,
            influxdb_client,
            uniq_id,
            num_tsdb_windows,
            opt_in_timeseries_buffer: Default::default(),
            rpc_secret_key_cache,
            tsdb_save_interval_seconds,
            tsdb_window,
            user_balance_cache,

            _flush_sender: flush_sender,
        };

        // any errors inside this task will cause the application to exit
        // TODO? change this to the X and XTask pattern like the latency crate uses
        let handle = tokio::spawn(async move {
            new.aggregate_and_save_loop(stat_receiver, shutdown_receiver, flush_receiver)
                .await
        });

        Ok(Some((stat_sender, handle).into()))
    }

    async fn aggregate_and_save_loop(
        &mut self,
        mut stat_receiver: mpsc::UnboundedReceiver<AppStat>,
        mut shutdown_receiver: broadcast::Receiver<()>,
        mut flush_receiver: mpsc::Receiver<oneshot::Sender<FlushedStats>>,
    ) -> Web3ProxyResult<()> {
        let mut tsdb_save_interval =
            interval(Duration::from_secs(self.tsdb_save_interval_seconds as u64));
        let mut db_save_interval =
            interval(Duration::from_secs(self.db_save_interval_seconds as u64));

        // TODO: this should be a FlushedStats that we add to
        let mut total_frontend_requests = 0;
        let mut tsdb_frontend_requests = 0;
        let mut tsdb_internal_requests = 0;
        let mut db_frontend_requests = 0;
        let mut db_internal_requests = 0;

        loop {
            select! {
                stat = stat_receiver.recv() => {
                    if let Some(stat) = stat {
                        total_frontend_requests += self._buffer_app_stat(stat).await?;

                        // TODO: if buffers are big, flush now?
                    } else {
                        break;
                    }
                }
                _ = db_save_interval.tick() => {
                    // TODO: tokio spawn this! (but with a semaphore on db_save_interval)
                    trace!("DB save internal tick");
                    let (count, new_frontend_requests, new_internal_requests) = self.save_relational_stats().await;
                    if count > 0 {
                        db_frontend_requests += new_frontend_requests;
                        db_internal_requests += new_internal_requests;
                        debug!("Saved {} stats for {} requests to the relational db", count, new_frontend_requests);
                    }
                }
                _ = tsdb_save_interval.tick() => {
                    trace!("TSDB save internal tick");
                    let (count, new_frontend_requests, new_internal_requests) = self.save_tsdb_stats().await;
                    if count > 0 {
                        tsdb_frontend_requests += new_frontend_requests;
                        tsdb_internal_requests += new_internal_requests;
                        debug!("Saved {} stats for {} requests to the tsdb @ {}/{}", count, new_frontend_requests, self.tsdb_window, self.num_tsdb_windows);
                    }
                }
                x = flush_receiver.recv() => {
                    match x {
                        Some(x) => {
                            let flushed_stats = self._flush(&mut stat_receiver).await?;

                            tsdb_frontend_requests += flushed_stats.timeseries_frontend_requests;
                            tsdb_internal_requests += flushed_stats.timeseries_internal_requests;

                            db_frontend_requests += flushed_stats.relational_frontend_requests;
                            db_internal_requests += flushed_stats.relational_internal_requests;

                            if let Err(err) = x.send(flushed_stats) {
                                error!(?flushed_stats, ?err, "unable to notify about flushed stats");
                            }
                        }
                        None => {
                            error!("unable to flush stat buffer!");
                            break;
                        }
                    }
                }
                x = shutdown_receiver.recv() => {
                    match x {
                        Ok(_) => {
                            info!("stat_loop shutting down");
                        },
                        Err(err) => error!("stat_loop shutdown receiver err={:?}", err),
                    }
                    break;
                }
            }
        }

        // TODO: wait on the frontend to shutdown
        // TODO: wait on all websockets to close
        // TODO: wait on all pending external requests to finish
        info!("waiting 5 seconds for remaining stats to arrive");
        sleep(Duration::from_secs(5)).await;

        // loop {
        //     // nope. this won't ever be true because we keep making stats for internal requests
        //     // if stat_receiver.is_disconnected() {
        //     //     info!("stat_receiver is disconnected");
        //     //     break;
        //     // }

        //     // TODO: don't just sleep. watch a channel
        //     sleep(Duration::from_millis(10)).await;
        // }

        let flushed_stats = self._flush(&mut stat_receiver).await?;

        tsdb_frontend_requests += flushed_stats.timeseries_frontend_requests;
        tsdb_internal_requests += flushed_stats.timeseries_internal_requests;

        db_frontend_requests += flushed_stats.relational_frontend_requests;
        db_internal_requests += flushed_stats.relational_internal_requests;

        // TODO: if these totals don't match, something is wrong! log something or maybe even return an error
        info!(%total_frontend_requests, %tsdb_frontend_requests, %tsdb_internal_requests, %db_frontend_requests, %db_internal_requests, "accounting and stat save loop complete");

        Ok(())
    }

    async fn _buffer_app_stat(&mut self, stat: AppStat) -> Web3ProxyResult<u64> {
        match stat {
            AppStat::RpcQuery(web3_request) => self._buffer_web3_request(web3_request).await,
        }
    }

    async fn _buffer_web3_request(&mut self, web3_request: Web3Request) -> Web3ProxyResult<u64> {
        // we convert on this side of the channel so that we don't slow down the request
        let stat = RpcQueryStats::try_from_metadata(web3_request)?;

        // update the latest balance
        // do this BEFORE emitting any stats
        let mut approximate_balance_remaining = 0.into();
        let mut active_premium = false;
        if let Ok(db_conn) = global_db_conn() {
            let user_id = stat.authorization.checks.user_id;

            // update the user's balance
            if user_id != 0 {
                // update the user's cached balance
                let mut user_balance = stat.authorization.checks.latest_balance.write().await;

                // TODO: move this to a helper function
                user_balance.total_frontend_requests += 1;
                user_balance.total_spent += stat.compute_unit_cost;

                if !stat.backend_rpcs_used.is_empty() {
                    user_balance.total_cache_misses += 1;
                }

                // if paid_credits_used is true, then they were premium at the start of the request
                if stat.authorization.checks.paid_credits_used {
                    // TODO: this lets them get a negative remaining balance. we should clear if close to 0
                    user_balance.total_spent_paid_credits += stat.compute_unit_cost;

                    // check if they still have premium
                    if user_balance.active_premium() {
                        // TODO: referall credits here? i think in the save_db section still makes sense for those
                        active_premium = true;
                    } else if let Err(err) = self
                        .user_balance_cache
                        .invalidate(&user_balance.user_id, &db_conn, &self.rpc_secret_key_cache)
                        .await
                    {
                        // was premium, but isn't anymore due to paying for this query. clear the cache
                        // TODO: stop at <$0.000001 instead of negative?
                        warn!(?err, "unable to clear caches");
                    }
                } else if user_balance.active_premium() {
                    active_premium = true;

                    if user_balance.downgrade_tier_id.is_some() {
                        // paid credits were not used, but now we have active premium. invalidate the caches
                        // TODO: this seems unliekly. should we warn if this happens so we can investigate?
                        if let Err(err) = self
                            .user_balance_cache
                            .invalidate(&user_balance.user_id, &db_conn, &self.rpc_secret_key_cache)
                            .await
                        {
                            // was premium, but isn't anymore due to paying for this query. clear the cache
                            // TODO: stop at <$0.000001 instead of negative?
                            warn!(?err, "unable to clear caches");
                        }
                    }
                }

                approximate_balance_remaining = user_balance.remaining();
            }

            let accounting_key = stat.accounting_key(self.billing_period_seconds);

            let span =
                tracing::trace_span!("accounting", key = tracing::field::debug(&accounting_key))
                    .or_current();

            self.accounting_db_buffer
                .entry(accounting_key)
                .or_default()
                .add(stat.clone(), None)
                .instrument(span)
                .await;
        }

        if self.influxdb_client.is_some() {
            if let Some(opt_in_timeseries_key) = stat.owned_timeseries_key(active_premium) {
                let span = tracing::trace_span!(
                    "owned_timeseries",
                    key = tracing::field::debug(&opt_in_timeseries_key)
                )
                .or_current();
                self.opt_in_timeseries_buffer
                    .entry(opt_in_timeseries_key)
                    .or_default()
                    .add(stat.clone(), Some(approximate_balance_remaining))
                    .instrument(span)
                    .await;
            }

            let global_timeseries_key = stat.global_timeseries_key();
            let span = tracing::trace_span!(
                "global_timeseries",
                key = tracing::field::debug(&global_timeseries_key)
            )
            .or_current();
            self.global_timeseries_buffer
                .entry(global_timeseries_key)
                .or_default()
                .add(stat, None)
                .instrument(span)
                .await;
        }

        Ok(1)
    }

    async fn _flush(
        &mut self,
        stat_receiver: &mut mpsc::UnboundedReceiver<AppStat>,
    ) -> Web3ProxyResult<FlushedStats> {
        trace!("flush");

        // fill the buffer
        while let Ok(stat) = stat_receiver.try_recv() {
            self._buffer_app_stat(stat).await?;
        }

        // flush the buffers
        // TODO: include frontend counts here
        let (timeseries_count, timeseries_frontend_requests, timeseries_internal_requests) =
            self.save_tsdb_stats().await;
        let (relational_count, relational_frontend_requests, relational_internal_requests) =
            self.save_relational_stats().await;

        // notify
        let flushed_stats = FlushedStats {
            timeseries: timeseries_count,
            timeseries_frontend_requests,
            timeseries_internal_requests,
            relational: relational_count,
            relational_frontend_requests,
            relational_internal_requests,
        };

        trace!(?flushed_stats);

        Ok(flushed_stats)
    }

    async fn save_relational_stats(&mut self) -> (usize, u64, u64) {
        let mut count = 0;
        let mut frontend_requests = 0;
        let mut internal_requests = 0;

        if let Ok(db_conn) = global_db_conn() {
            count = self.accounting_db_buffer.len();
            for (key, stat) in self.accounting_db_buffer.drain() {
                let new_frontend_requests = stat.frontend_requests;
                let is_internal = matches!(key.authorization_type, AuthorizationType::Internal);

                // TODO: batch saves
                // TODO: i don't like passing key (which came from the stat) to the function on the stat. but it works for now
                if let Err(err) = stat
                    .save_db(
                        self.chain_id,
                        &db_conn,
                        key,
                        &self.user_balance_cache,
                        &self.rpc_secret_key_cache,
                    )
                    .await
                {
                    // TODO: save the stat and retry later!
                    error!(?err, %count, %new_frontend_requests, %is_internal, "unable to save accounting entry!");
                } else if is_internal {
                    internal_requests += new_frontend_requests;
                } else {
                    frontend_requests += new_frontend_requests;
                };
            }
        }

        (count, frontend_requests, internal_requests)
    }

    // TODO: bucket should be an enum so that we don't risk typos
    // TODO: return type should be a struct so we dont mix up the values
    async fn save_tsdb_stats(&mut self) -> (usize, u64, u64) {
        let mut count = 0;
        let mut frontend_requests = 0;
        let mut internal_requests = 0;

        if let Some(influxdb_client) = self.influxdb_client.as_ref() {
            // every time we save, we increment the tsdb_window. this is used to ensure that stats don't overwrite others because the keys match
            // this has to be done carefully or cardinality becomes a problem!
            // https://docs.influxdata.com/influxdb/v2.0/write-data/best-practices/duplicate-points/
            self.tsdb_window += 1;
            if self.tsdb_window >= self.num_tsdb_windows {
                self.tsdb_window = 0;
            }

            let uniq = self.uniq_id + self.tsdb_window;

            let influxdb_bucket = self
                .influxdb_bucket
                .as_ref()
                .expect("if client is set, bucket must be set");

            // TODO: use stream::iter properly to avoid allocating this Vec
            let mut points = vec![];

            for (key, stat) in self.global_timeseries_buffer.drain() {
                // TODO: i don't like passing key (which came from the stat) to the function on the stat. but it works for now
                let new_frontend_requests = stat.frontend_requests;
                let is_internal = matches!(key.authorization_type, AuthorizationType::Internal);

                match stat
                    .build_timeseries_point("global_proxy", self.chain_id, key, uniq)
                    .await
                {
                    Ok(point) => {
                        points.push(point);

                        if is_internal {
                            internal_requests += new_frontend_requests;
                        } else {
                            frontend_requests += new_frontend_requests;
                        };
                    }
                    Err(err) => {
                        // TODO: what can cause this?
                        error!(?err, %new_frontend_requests, % is_internal, "unable to build global stat!");
                    }
                };
            }

            for (key, stat) in self.opt_in_timeseries_buffer.drain() {
                // TODO: i don't like passing key (which came from the stat) to the function on the stat. but it works for now
                match stat
                    .build_timeseries_point("opt_in_proxy", self.chain_id, key, uniq)
                    .await
                {
                    Ok(point) => {
                        points.push(point);
                    }
                    Err(err) => {
                        // TODO: what can cause this?
                        error!(?err, "unable to build opt-in stat!");
                    }
                };
            }

            count = points.len();

            if count > 0 {
                // TODO: put max_batch_size in config?
                // TODO: i think the real limit is the byte size of the http request. so, a simple line count won't work very well
                let max_batch_size = 1000;

                let mut num_left = count;

                while num_left > 0 {
                    let batch_size = num_left.min(max_batch_size);

                    // TODO: there has to be a better way to chunk this up. chunk on the stream with the stream being an iter?
                    let p = points.split_off(batch_size);

                    num_left -= batch_size;

                    if let Err(err) = influxdb_client
                        .write(influxdb_bucket, stream::iter(points))
                        .await
                    {
                        // TODO: if this errors, we throw away some of the pending stats! retry any failures! (but not successes. it can have partial successes!)
                        error!(?err, batch_size, "unable to save tsdb stats!");
                        // TODO: we should probably wait a second to give errors a chance to settle
                    }

                    points = p;
                }
            }
        }

        (count, frontend_requests, internal_requests)
    }
}
