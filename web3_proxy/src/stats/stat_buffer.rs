use super::{AppStat, RpcQueryKey};
use crate::app::{RpcSecretKeyCache, Web3ProxyJoinHandle};
use crate::frontend::errors::Web3ProxyResult;
use derive_more::From;
use futures::stream;
use hashbrown::HashMap;
use influxdb2::api::write::TimestampPrecision;
use log::{error, info, trace};
use migration::sea_orm::prelude::Decimal;
use migration::sea_orm::DatabaseConnection;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::interval;

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
    /// Balance tells us the user's balance at this point in time
    pub latest_balance: Decimal,
}

#[derive(From)]
pub struct SpawnedStatBuffer {
    pub stat_sender: flume::Sender<AppStat>,
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
    influxdb_client: Option<influxdb2::Client>,
    opt_in_timeseries_buffer: HashMap<RpcQueryKey, BufferedRpcQueryStats>,
    rpc_secret_key_cache: Option<RpcSecretKeyCache>,
    timestamp_precision: TimestampPrecision,
    tsdb_save_interval_seconds: u32,
}

impl StatBuffer {
    #[allow(clippy::too_many_arguments)]
    pub fn try_spawn(
        billing_period_seconds: i64,
        bucket: String,
        chain_id: u64,
        db_conn: Option<DatabaseConnection>,
        db_save_interval_seconds: u32,
        influxdb_client: Option<influxdb2::Client>,
        rpc_secret_key_cache: Option<RpcSecretKeyCache>,
        shutdown_receiver: broadcast::Receiver<()>,
        tsdb_save_interval_seconds: u32,
    ) -> anyhow::Result<Option<SpawnedStatBuffer>> {
        if db_conn.is_none() && influxdb_client.is_none() {
            return Ok(None);
        }

        let (stat_sender, stat_receiver) = flume::unbounded();

        let timestamp_precision = TimestampPrecision::Seconds;
        let mut new = Self {
            accounting_db_buffer: Default::default(),
            billing_period_seconds,
            chain_id,
            db_conn,
            db_save_interval_seconds,
            global_timeseries_buffer: Default::default(),
            influxdb_client,
            opt_in_timeseries_buffer: Default::default(),
            rpc_secret_key_cache,
            timestamp_precision,
            tsdb_save_interval_seconds,
        };

        // any errors inside this task will cause the application to exit
        let handle = tokio::spawn(async move {
            new.aggregate_and_save_loop(bucket, stat_receiver, shutdown_receiver)
                .await
        });

        Ok(Some((stat_sender, handle).into()))
    }

    async fn aggregate_and_save_loop(
        &mut self,
        bucket: String,
        stat_receiver: flume::Receiver<AppStat>,
        mut shutdown_receiver: broadcast::Receiver<()>,
    ) -> Web3ProxyResult<()> {
        let mut tsdb_save_interval =
            interval(Duration::from_secs(self.tsdb_save_interval_seconds as u64));
        let mut db_save_interval =
            interval(Duration::from_secs(self.db_save_interval_seconds as u64));

        // TODO: Somewhere here we should probably be updating the balance of the user
        // And also update the credits used etc. for the referred user

        loop {
            tokio::select! {
                stat = stat_receiver.recv_async() => {
                    // trace!("Received stat");
                    // save the stat to a buffer
                    match stat {
                        Ok(AppStat::RpcQuery(stat)) => {
                            if self.influxdb_client.is_some() {
                                // TODO: round the timestamp at all?

                                let global_timeseries_key = stat.global_timeseries_key();

                                self.global_timeseries_buffer.entry(global_timeseries_key).or_default().add(stat.clone());

                                if let Some(opt_in_timeseries_key) = stat.opt_in_timeseries_key() {
                                    self.opt_in_timeseries_buffer.entry(opt_in_timeseries_key).or_default().add(stat.clone());
                                }
                            }

                            if self.db_conn.is_some() {
                                self.accounting_db_buffer.entry(stat.accounting_key(self.billing_period_seconds)).or_default().add(stat);
                            }
                        }
                        Err(err) => {
                            info!("error receiving stat: {}", err);
                            break;
                        }
                    }
                }
                _ = db_save_interval.tick() => {
                    // info!("DB save internal tick");
                    let count = self.save_relational_stats().await;
                    if count > 0 {
                        trace!("Saved {} stats to the relational db", count);
                    }
                }
                _ = tsdb_save_interval.tick() => {
                    // info!("TSDB save internal tick");
                    let count = self.save_tsdb_stats(&bucket).await;
                    if count > 0 {
                        trace!("Saved {} stats to the tsdb", count);
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

        let saved_relational = self.save_relational_stats().await;

        info!("saved {} pending relational stat(s)", saved_relational);

        let saved_tsdb = self.save_tsdb_stats(&bucket).await;

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
                        self.rpc_secret_key_cache.as_ref(),
                    )
                    .await
                {
                    error!("unable to save accounting entry! err={:?}", err);
                };
            }
        }

        count
    }

    // TODO: bucket should be an enum so that we don't risk typos
    async fn save_tsdb_stats(&mut self, bucket: &str) -> usize {
        let mut count = 0;

        if let Some(influxdb_client) = self.influxdb_client.as_ref() {
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
                        error!("unable to build global stat! err={:?}", err);
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
                            bucket,
                            stream::iter(points),
                            self.timestamp_precision,
                        )
                        .await
                    {
                        // TODO: if this errors, we throw away some of the pending stats! we should probably buffer them somewhere to be tried again
                        error!("unable to save {} tsdb stats! err={:?}", batch_size, err);
                    }

                    points = p;
                }
            }
        }

        count
    }
}
