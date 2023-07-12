use super::{AppStat, FlushedStats, RpcQueryKey};
use crate::app::Web3ProxyJoinHandle;
use crate::caches::{RpcSecretKeyCache, UserBalanceCache};
use crate::errors::Web3ProxyResult;
use crate::stats::RpcQueryStats;
use derive_more::From;
use futures::stream;
use hashbrown::HashMap;
use influxdb2::api::write::TimestampPrecision;
use migration::sea_orm::prelude::Decimal;
use migration::sea_orm::DatabaseConnection;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::{interval, sleep};
use tracing::{error, info, trace, warn};

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
    pub approximate_balance_remaining: Decimal,
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
    db_conn: Option<DatabaseConnection>,
    db_save_interval_seconds: u32,
    global_timeseries_buffer: HashMap<RpcQueryKey, BufferedRpcQueryStats>,
    influxdb_bucket: Option<String>,
    influxdb_client: Option<influxdb2::Client>,
    opt_in_timeseries_buffer: HashMap<RpcQueryKey, BufferedRpcQueryStats>,
    rpc_secret_key_cache: RpcSecretKeyCache,
    timestamp_precision: TimestampPrecision,
    tsdb_save_interval_seconds: u32,
    user_balance_cache: UserBalanceCache,

    _flush_sender: mpsc::Sender<oneshot::Sender<FlushedStats>>,
}

impl StatBuffer {
    #[allow(clippy::too_many_arguments)]
    pub fn try_spawn(
        billing_period_seconds: i64,
        chain_id: u64,
        db_conn: Option<DatabaseConnection>,
        db_save_interval_seconds: u32,
        influxdb_bucket: Option<String>,
        mut influxdb_client: Option<influxdb2::Client>,
        rpc_secret_key_cache: Option<RpcSecretKeyCache>,
        user_balance_cache: Option<UserBalanceCache>,
        shutdown_receiver: broadcast::Receiver<()>,
        tsdb_save_interval_seconds: u32,
        flush_sender: mpsc::Sender<oneshot::Sender<FlushedStats>>,
        flush_receiver: mpsc::Receiver<oneshot::Sender<FlushedStats>>,
    ) -> anyhow::Result<Option<SpawnedStatBuffer>> {
        if influxdb_bucket.is_none() {
            influxdb_client = None;
        }

        if db_conn.is_none() && influxdb_client.is_none() {
            return Ok(None);
        }

        let (stat_sender, stat_receiver) = mpsc::unbounded_channel();

        let timestamp_precision = TimestampPrecision::Seconds;

        let mut new = Self {
            accounting_db_buffer: Default::default(),
            billing_period_seconds,
            chain_id,
            db_conn,
            db_save_interval_seconds,
            global_timeseries_buffer: Default::default(),
            influxdb_bucket,
            influxdb_client,
            opt_in_timeseries_buffer: Default::default(),
            rpc_secret_key_cache: rpc_secret_key_cache.unwrap(),
            timestamp_precision,
            tsdb_save_interval_seconds,
            user_balance_cache: user_balance_cache.unwrap(),
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

        loop {
            tokio::select! {
                stat = stat_receiver.recv() => {
                    // trace!("Received stat");
                    // save the stat to a buffer

                    // TODO: tokio spawn this!
                    match stat {
                        Some(AppStat::RpcQuery(request_metadata)) => {
                            // we convert on this side of the channel so that we don't slow down the request
                            let stat = RpcQueryStats::try_from_metadata(request_metadata)?;

                            // update the latest balance
                            // do this BEFORE emitting any stats
                            let mut approximate_balance_remaining = 0.into();
                            if let Some(db_conn) = self.db_conn.as_ref() {
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
                                        } else if let Err(err) = self.user_balance_cache.invalidate(&user_balance.user_id, db_conn, &self.rpc_secret_key_cache).await {
                                            // was premium, but isn't anymore due to paying for this query. clear the cache
                                            // TODO: stop at <$0.000001 instead of negative?
                                            warn!(?err, "unable to clear caches");
                                        }
                                    } else if user_balance.active_premium() {
                                        // paid credits were not used, but now we have active premium. invalidate the caches
                                        // TODO: this seems unliekly. should we warn if this happens so we can investigate?
                                        if let Err(err) = self.user_balance_cache.invalidate(&user_balance.user_id, db_conn, &self.rpc_secret_key_cache).await {
                                            // was premium, but isn't anymore due to paying for this query. clear the cache
                                            // TODO: stop at <$0.000001 instead of negative?
                                            warn!(?err, "unable to clear caches");
                                        }
                                    }

                                    approximate_balance_remaining = user_balance.remaining();
                                }

                                self.accounting_db_buffer.entry(stat.accounting_key(self.billing_period_seconds)).or_default().add(stat.clone(), approximate_balance_remaining).await;
                            }

                            if self.influxdb_client.is_some() {
                                // TODO: round the timestamp at all?

                                if let Some(opt_in_timeseries_key) = stat.owned_timeseries_key() {
                                    self.opt_in_timeseries_buffer.entry(opt_in_timeseries_key).or_default().add(stat.clone(), approximate_balance_remaining).await;
                                }

                                let global_timeseries_key = stat.global_timeseries_key();

                                self.global_timeseries_buffer.entry(global_timeseries_key).or_default().add(stat, approximate_balance_remaining).await;
                            }
                        }
                        None => {
                            info!("error receiving stat");
                            break;
                        }
                    }
                }
                _ = db_save_interval.tick() => {
                    // TODO: tokio spawn this! (but with a semaphore on db_save_interval)
                    trace!("DB save internal tick");
                    let count = self.save_relational_stats().await;
                    if count > 0 {
                        trace!("Saved {} stats to the relational db", count);
                    }
                }
                _ = tsdb_save_interval.tick() => {
                    trace!("TSDB save internal tick");
                    let count = self.save_tsdb_stats().await;
                    if count > 0 {
                        trace!("Saved {} stats to the tsdb", count);
                    }
                }
                x = flush_receiver.recv() => {
                    match x {
                        Some(x) => {
                            trace!("flush");

                            let tsdb_count = self.save_tsdb_stats().await;

                            let relational_count = self.save_relational_stats().await;

                            let flushed_stats = FlushedStats{ timeseries: tsdb_count, relational: relational_count};

                            trace!(?flushed_stats);

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

        // TODO: wait on all websockets to close
        // TODO: wait on all pending external requests to finish
        info!("waiting 10 seconds for remaining stats to arrive");
        sleep(Duration::from_secs(10)).await;

        // loop {
        //     // nope. this won't ever be true because we keep making stats for internal requests
        //     // if stat_receiver.is_disconnected() {
        //     //     info!("stat_receiver is disconnected");
        //     //     break;
        //     // }

        //     // TODO: don't just sleep. watch a channel
        //     sleep(Duration::from_millis(10)).await;
        // }

        let saved_relational = self.save_relational_stats().await;

        info!("saved {} pending relational stat(s)", saved_relational);

        let saved_tsdb = self.save_tsdb_stats().await;

        info!("saved {} pending tsdb stat(s)", saved_tsdb);

        info!("accounting and stat save loop complete");

        Ok(())
    }

    async fn save_relational_stats(&mut self) -> usize {
        let mut count = 0;

        if let Some(db_conn) = self.db_conn.as_ref() {
            count = self.accounting_db_buffer.len();
            for (key, stat) in self.accounting_db_buffer.drain() {
                // TODO: batch saves
                // TODO: i don't like passing key (which came from the stat) to the function on the stat. but it works for now
                if let Err(err) = stat
                    .save_db(
                        self.chain_id,
                        db_conn,
                        key,
                        &self.user_balance_cache,
                        &self.rpc_secret_key_cache,
                    )
                    .await
                {
                    error!(?err, "unable to save accounting entry!");
                };
            }
        }

        count
    }

    // TODO: bucket should be an enum so that we don't risk typos
    async fn save_tsdb_stats(&mut self) -> usize {
        let mut count = 0;

        if let Some(influxdb_client) = self.influxdb_client.as_ref() {
            let influxdb_bucket = self
                .influxdb_bucket
                .as_ref()
                .expect("if client is set, bucket must be set");

            // TODO: use stream::iter properly to avoid allocating this Vec
            let mut points = vec![];

            for (key, stat) in self.global_timeseries_buffer.drain() {
                // TODO: i don't like passing key (which came from the stat) to the function on the stat. but it works for now
                match stat
                    .build_timeseries_point("global_proxy", self.chain_id, key)
                    .await
                {
                    Ok(point) => {
                        points.push(point);
                    }
                    Err(err) => {
                        error!(?err, "unable to build global stat!");
                    }
                };
            }

            for (key, stat) in self.opt_in_timeseries_buffer.drain() {
                // TODO: i don't like passing key (which came from the stat) to the function on the stat. but it works for now
                match stat
                    .build_timeseries_point("opt_in_proxy", self.chain_id, key)
                    .await
                {
                    Ok(point) => {
                        points.push(point);
                    }
                    Err(err) => {
                        // TODO: if this errors, we throw away some of the pending stats! we should probably buffer them somewhere to be tried again
                        error!("unable to build opt-in stat! err={:?}", err);
                    }
                };
            }

            count = points.len();

            if count > 0 {
                // TODO: put max_batch_size in config?
                // TODO: i think the real limit is the byte size of the http request. so, a simple line count won't work very well
                let max_batch_size = 100;

                let mut num_left = count;

                while num_left > 0 {
                    let batch_size = num_left.min(max_batch_size);

                    // TODO: there has to be a better way to chunk this up. chunk on the stream with the stream being an iter?
                    let p = points.split_off(batch_size);

                    num_left -= batch_size;

                    if let Err(err) = influxdb_client
                        .write_with_precision(
                            influxdb_bucket,
                            stream::iter(points),
                            self.timestamp_precision,
                        )
                        .await
                    {
                        // TODO: if this errors, we throw away some of the pending stats! we should probably buffer them somewhere to be tried again
                        error!(?err, batch_size, "unable to save tsdb stats!");
                        // TODO: we should probably wait a second to give errors a chance to settle
                    }

                    points = p;
                }
            }
        }

        count
    }
}
