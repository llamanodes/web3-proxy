//! Store "stats" in a database for billing and a different database for graphing
//!
//! TODO: move some of these structs/functions into their own file?
pub mod db_queries;
pub mod influxdb_queries;

use crate::frontend::authorization::{Authorization, RequestMetadata};
use axum::headers::Origin;
use chrono::{TimeZone, Utc};
use derive_more::From;
use entities::rpc_accounting_v2;
use entities::sea_orm_active_enums::TrackingLevel;
use futures::stream;
use hashbrown::HashMap;
use influxdb2::api::write::TimestampPrecision;
use influxdb2::models::DataPoint;
use log::{error, info, trace};
use migration::sea_orm::{self, DatabaseConnection, EntityTrait};
use migration::{Expr, OnConflict};
use std::num::NonZeroU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::interval;

pub enum StatType {
    Aggregated,
    Detailed,
}

// Pub is needed for migration ... I could also write a second constructor for this if needed
/// TODO: better name?
#[derive(Clone, Debug)]
pub struct RpcQueryStats {
    pub authorization: Arc<Authorization>,
    pub method: Option<String>,
    pub archive_request: bool,
    pub error_response: bool,
    pub request_bytes: u64,
    /// if backend_requests is 0, there was a cache_hit
    // pub frontend_request: u64,
    pub backend_requests: u64,
    pub response_bytes: u64,
    pub response_millis: u64,
    pub response_timestamp: i64,
}

#[derive(Clone, From, Hash, PartialEq, Eq)]
pub struct RpcQueryKey {
    /// unix epoch time
    /// for the time series db, this is (close to) the time that the response was sent
    /// for the account database, this is rounded to the week
    response_timestamp: i64,
    /// true if an archive server was needed to serve the request
    archive_needed: bool,
    /// true if the response was some sort of JSONRPC error
    error_response: bool,
    /// method tracking is opt-in
    method: Option<String>,
    /// origin tracking is opt-in
    origin: Option<Origin>,
    /// None if the public url was used
    rpc_secret_key_id: Option<NonZeroU64>,
}

/// round the unix epoch time to the start of a period
fn round_timestamp(timestamp: i64, period_seconds: i64) -> i64 {
    timestamp / period_seconds * period_seconds
}

impl RpcQueryStats {
    /// rpc keys can opt into multiple levels of tracking.
    /// we always need enough to handle billing, so even the "none" level still has some minimal tracking.
    /// This "accounting_key" is used in the relational database.
    /// anonymous users are also saved in the relational database so that the host can do their own cost accounting.
    fn accounting_key(&self, period_seconds: i64) -> RpcQueryKey {
        let response_timestamp = round_timestamp(self.response_timestamp, period_seconds);

        let rpc_secret_key_id = self.authorization.checks.rpc_secret_key_id;

        let (method, origin) = match self.authorization.checks.tracking_level {
            TrackingLevel::None => {
                // this RPC key requested no tracking. this is the default
                // do not store the method or the origin
                (None, None)
            }
            TrackingLevel::Aggregated => {
                // this RPC key requested tracking aggregated across all methods and origins
                // TODO: think about this more. do we want the origin or not? grouping free cost per site might be useful. i'd rather not collect things if we don't have a planned purpose though
                let method = None;
                let origin = None;

                (method, origin)
            }
            TrackingLevel::Detailed => {
                // detailed tracking keeps track of the method and origin
                // depending on the request, the origin might still be None
                let method = self.method.clone();
                let origin = self.authorization.origin.clone();

                (method, origin)
            }
        };

        RpcQueryKey {
            response_timestamp,
            archive_needed: self.archive_request,
            error_response: self.error_response,
            method,
            rpc_secret_key_id,
            origin,
        }
    }

    /// all rpc keys are aggregated in the global stats
    /// TODO: should we store "anon" or "registered" as a key just to be able to split graphs?
    fn global_timeseries_key(&self) -> RpcQueryKey {
        // we include the method because that can be helpful for predicting load
        let method = self.method.clone();
        // we don't store origin in the timeseries db. its only used for optional accounting
        let origin = None;
        // everyone gets grouped together
        let rpc_secret_key_id = None;

        RpcQueryKey {
            response_timestamp: self.response_timestamp,
            archive_needed: self.archive_request,
            error_response: self.error_response,
            method,
            rpc_secret_key_id,
            origin,
        }
    }

    /// rpc keys can opt into more detailed tracking
    fn opt_in_timeseries_key(&self) -> Option<RpcQueryKey> {
        // we don't store origin in the timeseries db. its only optionaly used for accounting
        let origin = None;

        let (method, rpc_secret_key_id) = match self.authorization.checks.tracking_level {
            TrackingLevel::None => {
                // this RPC key requested no tracking. this is the default.
                return None;
            }
            TrackingLevel::Aggregated => {
                // this RPC key requested tracking aggregated across all methods
                (None, self.authorization.checks.rpc_secret_key_id)
            }
            TrackingLevel::Detailed => {
                // detailed tracking keeps track of the method
                (
                    self.method.clone(),
                    self.authorization.checks.rpc_secret_key_id,
                )
            }
        };

        let key = RpcQueryKey {
            response_timestamp: self.response_timestamp,
            archive_needed: self.archive_request,
            error_response: self.error_response,
            method,
            rpc_secret_key_id,
            origin,
        };

        Some(key)
    }
}

#[derive(Default)]
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
}

/// A stat that we aggregate and then store in a database.
/// For now there is just one, but I think there might be others later
#[derive(Debug, From)]
pub enum AppStat {
    RpcQuery(RpcQueryStats),
}

#[derive(From)]
pub struct SpawnedStatBuffer {
    pub stat_sender: flume::Sender<AppStat>,
    /// these handles are important and must be allowed to finish
    pub background_handle: JoinHandle<anyhow::Result<()>>,
}

pub struct StatBuffer {
    chain_id: u64,
    db_conn: Option<DatabaseConnection>,
    influxdb_client: Option<influxdb2::Client>,
    tsdb_save_interval_seconds: u32,
    db_save_interval_seconds: u32,
    billing_period_seconds: i64,
    global_timeseries_buffer: HashMap<RpcQueryKey, BufferedRpcQueryStats>,
    opt_in_timeseries_buffer: HashMap<RpcQueryKey, BufferedRpcQueryStats>,
    accounting_db_buffer: HashMap<RpcQueryKey, BufferedRpcQueryStats>,
    timestamp_precision: TimestampPrecision,
}

impl BufferedRpcQueryStats {
    fn add(&mut self, stat: RpcQueryStats) {
        // a stat always come from just 1 frontend request
        self.frontend_requests += 1;

        if stat.backend_requests == 0 {
            // no backend request. cache hit!
            self.cache_hits += 1;
        } else {
            // backend requests! cache miss!
            self.cache_misses += 1;

            // a single frontend request might have multiple backend requests
            self.backend_requests += stat.backend_requests;
        }

        self.sum_request_bytes += stat.request_bytes;
        self.sum_response_bytes += stat.response_bytes;
        self.sum_response_millis += stat.response_millis;
    }

    // TODO: take a db transaction instead so that we can batch?
    async fn save_db(
        self,
        chain_id: u64,
        db_conn: &DatabaseConnection,
        key: RpcQueryKey,
    ) -> anyhow::Result<()> {
        let period_datetime = Utc.timestamp_opt(key.response_timestamp, 0).unwrap();

        // this is a lot of variables
        let accounting_entry = rpc_accounting_v2::ActiveModel {
            id: sea_orm::NotSet,
            rpc_key_id: sea_orm::Set(key.rpc_secret_key_id.map(Into::into).unwrap_or_default()),
            origin: sea_orm::Set(key.origin.map(|x| x.to_string()).unwrap_or_default()),
            chain_id: sea_orm::Set(chain_id),
            period_datetime: sea_orm::Set(period_datetime),
            method: sea_orm::Set(key.method.unwrap_or_default()),
            archive_needed: sea_orm::Set(key.archive_needed),
            error_response: sea_orm::Set(key.error_response),
            frontend_requests: sea_orm::Set(self.frontend_requests),
            backend_requests: sea_orm::Set(self.backend_requests),
            backend_retries: sea_orm::Set(self.backend_retries),
            no_servers: sea_orm::Set(self.no_servers),
            cache_misses: sea_orm::Set(self.cache_misses),
            cache_hits: sea_orm::Set(self.cache_hits),
            sum_request_bytes: sea_orm::Set(self.sum_request_bytes),
            sum_response_millis: sea_orm::Set(self.sum_response_millis),
            sum_response_bytes: sea_orm::Set(self.sum_response_bytes),
        };

        rpc_accounting_v2::Entity::insert(accounting_entry)
            .on_conflict(
                OnConflict::new()
                    .values([
                        (
                            rpc_accounting_v2::Column::FrontendRequests,
                            Expr::col(rpc_accounting_v2::Column::FrontendRequests)
                                .add(self.frontend_requests),
                        ),
                        (
                            rpc_accounting_v2::Column::BackendRequests,
                            Expr::col(rpc_accounting_v2::Column::BackendRequests)
                                .add(self.backend_requests),
                        ),
                        (
                            rpc_accounting_v2::Column::BackendRetries,
                            Expr::col(rpc_accounting_v2::Column::BackendRetries)
                                .add(self.backend_retries),
                        ),
                        (
                            rpc_accounting_v2::Column::NoServers,
                            Expr::col(rpc_accounting_v2::Column::NoServers).add(self.no_servers),
                        ),
                        (
                            rpc_accounting_v2::Column::CacheMisses,
                            Expr::col(rpc_accounting_v2::Column::CacheMisses)
                                .add(self.cache_misses),
                        ),
                        (
                            rpc_accounting_v2::Column::CacheHits,
                            Expr::col(rpc_accounting_v2::Column::CacheHits).add(self.cache_hits),
                        ),
                        (
                            rpc_accounting_v2::Column::SumRequestBytes,
                            Expr::col(rpc_accounting_v2::Column::SumRequestBytes)
                                .add(self.sum_request_bytes),
                        ),
                        (
                            rpc_accounting_v2::Column::SumResponseMillis,
                            Expr::col(rpc_accounting_v2::Column::SumResponseMillis)
                                .add(self.sum_response_millis),
                        ),
                        (
                            rpc_accounting_v2::Column::SumResponseBytes,
                            Expr::col(rpc_accounting_v2::Column::SumResponseBytes)
                                .add(self.sum_response_bytes),
                        ),
                    ])
                    .to_owned(),
            )
            .exec(db_conn)
            .await?;

        Ok(())
    }

    async fn build_timeseries_point(
        self,
        measurement: &str,
        chain_id: u64,
        key: RpcQueryKey,
    ) -> anyhow::Result<DataPoint> {
        let mut builder = DataPoint::builder(measurement);

        builder = builder.tag("chain_id", chain_id.to_string());

        if let Some(rpc_secret_key_id) = key.rpc_secret_key_id {
            builder = builder.tag("rpc_secret_key_id", rpc_secret_key_id.to_string());
        }

        if let Some(method) = key.method {
            builder = builder.tag("method", method);
        }

        builder = builder
            .tag("archive_needed", key.archive_needed.to_string())
            .tag("error_response", key.error_response.to_string())
            .field("frontend_requests", self.frontend_requests as i64)
            .field("backend_requests", self.backend_requests as i64)
            .field("no_servers", self.no_servers as i64)
            .field("cache_misses", self.cache_misses as i64)
            .field("cache_hits", self.cache_hits as i64)
            .field("sum_request_bytes", self.sum_request_bytes as i64)
            .field("sum_response_millis", self.sum_response_millis as i64)
            .field("sum_response_bytes", self.sum_response_bytes as i64);

        builder = builder.timestamp(key.response_timestamp);

        let point = builder.build()?;

        Ok(point)
    }
}

impl RpcQueryStats {
    pub fn new(
        method: Option<String>,
        authorization: Arc<Authorization>,
        metadata: Arc<RequestMetadata>,
        response_bytes: usize,
    ) -> Self {
        // TODO: try_unwrap the metadata to be sure that all the stats for this request have been collected
        // TODO: otherwise, i think the whole thing should be in a single lock that we can "reset" when a stat is created

        let archive_request = metadata.archive_request.load(Ordering::Acquire);
        let backend_requests = metadata.backend_requests.lock().len() as u64;
        let request_bytes = metadata.request_bytes;
        let error_response = metadata.error_response.load(Ordering::Acquire);
        let response_millis = metadata.start_instant.elapsed().as_millis() as u64;
        let response_bytes = response_bytes as u64;

        let response_timestamp = Utc::now().timestamp();

        Self {
            authorization,
            archive_request,
            method,
            backend_requests,
            request_bytes,
            error_response,
            response_bytes,
            response_millis,
            response_timestamp,
        }
    }

    /// Only used for migration from stats_v1 to stats_v2/v3
    pub fn modify_struct(
        &mut self,
        response_millis: u64,
        response_timestamp: i64,
        backend_requests: u64,
    ) {
        self.response_millis = response_millis;
        self.response_timestamp = response_timestamp;
        self.backend_requests = backend_requests;
    }
}

impl StatBuffer {
    #[allow(clippy::too_many_arguments)]
    pub fn try_spawn(
        chain_id: u64,
        bucket: String,
        db_conn: Option<DatabaseConnection>,
        influxdb_client: Option<influxdb2::Client>,
        db_save_interval_seconds: u32,
        tsdb_save_interval_seconds: u32,
        billing_period_seconds: i64,
        shutdown_receiver: broadcast::Receiver<()>,
    ) -> anyhow::Result<Option<SpawnedStatBuffer>> {
        if db_conn.is_none() && influxdb_client.is_none() {
            return Ok(None);
        }

        let (stat_sender, stat_receiver) = flume::unbounded();

        let timestamp_precision = TimestampPrecision::Seconds;
        let mut new = Self {
            chain_id,
            db_conn,
            influxdb_client,
            db_save_interval_seconds,
            tsdb_save_interval_seconds,
            billing_period_seconds,
            global_timeseries_buffer: Default::default(),
            opt_in_timeseries_buffer: Default::default(),
            accounting_db_buffer: Default::default(),
            timestamp_precision,
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
    ) -> anyhow::Result<()> {
        let mut tsdb_save_interval =
            interval(Duration::from_secs(self.tsdb_save_interval_seconds as u64));
        let mut db_save_interval =
            interval(Duration::from_secs(self.db_save_interval_seconds as u64));

        // TODO: Somewhere here we should probably be updating the balance of the user
        // And also update the credits used etc. for the referred user

        loop {
            tokio::select! {
                stat = stat_receiver.recv_async() => {
                    // info!("Received stat");
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
                            error!("error receiving stat: {:?}", err);
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
                    info!("shutdown signal ---");
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

        info!("saved {} pending relational stats", saved_relational);

        let saved_tsdb = self.save_tsdb_stats(&bucket).await;

        info!("saved {} pending tsdb stats", saved_tsdb);

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
                if let Err(err) = stat.save_db(self.chain_id, db_conn, key).await {
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

                    let p = points.split_off(batch_size);

                    num_left -= batch_size;

                    if let Err(err) = influxdb_client
                        .write_with_precision(bucket, stream::iter(p), self.timestamp_precision)
                        .await
                    {
                        // TODO: if this errors, we throw away some of the pending stats! we should probably buffer them somewhere to be tried again
                        error!("unable to save {} tsdb stats! err={:?}", batch_size, err);
                    }
                }
            }
        }

        count
    }
}
