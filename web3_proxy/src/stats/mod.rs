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
use log::{error, info};
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

/// TODO: better name?
#[derive(Clone, Debug)]
pub struct RpcQueryStats {
    authorization: Arc<Authorization>,
    method: String,
    archive_request: bool,
    error_response: bool,
    request_bytes: u64,
    /// if backend_requests is 0, there was a cache_hit
    backend_requests: u64,
    response_bytes: u64,
    response_millis: u64,
    response_timestamp: i64,
}

#[derive(Clone, From, Hash, PartialEq, Eq)]
struct RpcQueryKey {
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
                let method = Some(self.method.clone());
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

    /// all queries are aggregated
    /// TODO: should we store "anon" or "registered" as a key just to be able to split graphs?
    fn global_timeseries_key(&self) -> RpcQueryKey {
        let method = Some(self.method.clone());
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

    fn opt_in_timeseries_key(&self) -> RpcQueryKey {
        // we don't store origin in the timeseries db. its only optionaly used for accounting
        let origin = None;

        let (method, rpc_secret_key_id) = match self.authorization.checks.tracking_level {
            TrackingLevel::None => {
                // this RPC key requested no tracking. this is the default.
                // we still want graphs though, so we just use None as the rpc_secret_key_id
                (Some(self.method.clone()), None)
            }
            TrackingLevel::Aggregated => {
                // this RPC key requested tracking aggregated across all methods
                (None, self.authorization.checks.rpc_secret_key_id)
            }
            TrackingLevel::Detailed => {
                // detailed tracking keeps track of the method
                (
                    Some(self.method.clone()),
                    self.authorization.checks.rpc_secret_key_id,
                )
            }
        };

        RpcQueryKey {
            response_timestamp: self.response_timestamp,
            archive_needed: self.archive_request,
            error_response: self.error_response,
            method,
            rpc_secret_key_id,
            origin,
        }
    }
}

#[derive(Default)]
pub struct BufferedRpcQueryStats {
    frontend_requests: u64,
    backend_requests: u64,
    backend_retries: u64,
    no_servers: u64,
    cache_misses: u64,
    cache_hits: u64,
    sum_request_bytes: u64,
    sum_response_bytes: u64,
    sum_response_millis: u64,
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
        let period_datetime = Utc.timestamp_opt(key.response_timestamp as i64, 0).unwrap();

        // this is a lot of variables
        let accounting_entry = rpc_accounting_v2::ActiveModel {
            id: sea_orm::NotSet,
            rpc_key_id: sea_orm::Set(key.rpc_secret_key_id.map(Into::into)),
            origin: sea_orm::Set(key.origin.map(|x| x.to_string())),
            chain_id: sea_orm::Set(chain_id),
            period_datetime: sea_orm::Set(period_datetime),
            method: sea_orm::Set(key.method),
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

    // TODO: change this to return a DataPoint?
    async fn save_timeseries(
        self,
        bucket: &str,
        measurement: &str,
        chain_id: u64,
        influxdb2_clent: &influxdb2::Client,
        key: RpcQueryKey,
    ) -> anyhow::Result<()> {
        // TODO: error if key.origin is set?

        // TODO: what name?
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
        let timestamp_precision = TimestampPrecision::Seconds;

        let points = [builder.build()?];

        // TODO: bucket should be an enum so that we don't risk typos
        influxdb2_clent
            .write_with_precision(bucket, stream::iter(points), timestamp_precision)
            .await?;

        Ok(())
    }
}

impl RpcQueryStats {
    pub fn new(
        method: String,
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
}

impl StatBuffer {
    pub fn try_spawn(
        chain_id: u64,
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

        let mut new = Self {
            chain_id,
            db_conn,
            influxdb_client,
            db_save_interval_seconds,
            tsdb_save_interval_seconds,
            billing_period_seconds,
        };

        // any errors inside this task will cause the application to exit
        let handle = tokio::spawn(async move {
            new.aggregate_and_save_loop(stat_receiver, shutdown_receiver)
                .await
        });

        Ok(Some((stat_sender, handle).into()))
    }

    async fn aggregate_and_save_loop(
        &mut self,
        stat_receiver: flume::Receiver<AppStat>,
        mut shutdown_receiver: broadcast::Receiver<()>,
    ) -> anyhow::Result<()> {
        let mut tsdb_save_interval =
            interval(Duration::from_secs(self.tsdb_save_interval_seconds as u64));
        let mut db_save_interval =
            interval(Duration::from_secs(self.db_save_interval_seconds as u64));

        // TODO: this is used for rpc_accounting_v2 and influxdb. give it a name to match that? "stat" of some kind?
        let mut global_timeseries_buffer = HashMap::<RpcQueryKey, BufferedRpcQueryStats>::new();
        let mut opt_in_timeseries_buffer = HashMap::<RpcQueryKey, BufferedRpcQueryStats>::new();
        let mut accounting_db_buffer = HashMap::<RpcQueryKey, BufferedRpcQueryStats>::new();

        loop {
            tokio::select! {
                stat = stat_receiver.recv_async() => {
                    // save the stat to a buffer
                    match stat {
                        Ok(AppStat::RpcQuery(stat)) => {
                            if self.influxdb_client.is_some() {
                                // TODO: round the timestamp at all?

                                let global_timeseries_key = stat.global_timeseries_key();

                                global_timeseries_buffer.entry(global_timeseries_key).or_default().add(stat.clone());

                                let opt_in_timeseries_key =  stat.opt_in_timeseries_key();

                                opt_in_timeseries_buffer.entry(opt_in_timeseries_key).or_default().add(stat.clone());
                            }

                            if self.db_conn.is_some() {
                                accounting_db_buffer.entry(stat.accounting_key(self.billing_period_seconds)).or_default().add(stat);
                            }
                        }
                        Err(err) => {
                            error!("error receiving stat: {:?}", err);
                            break;
                        }
                    }
                }
                _ = db_save_interval.tick() => {
                    let db_conn = self.db_conn.as_ref().expect("db connection should always exist if there are buffered stats");

                    // TODO: batch saves
                    for (key, stat) in accounting_db_buffer.drain() {
                        // TODO: i don't like passing key (which came from the stat) to the function on the stat. but it works for now
                        if let Err(err) = stat.save_db(self.chain_id, db_conn, key).await {
                            error!("unable to save accounting entry! err={:?}", err);
                        };
                    }
                }
                _ = tsdb_save_interval.tick() => {
                    // TODO: batch saves
                    // TODO: better bucket names
                    let influxdb_client = self.influxdb_client.as_ref().expect("influxdb client should always exist if there are buffered stats");

                    for (key, stat) in global_timeseries_buffer.drain() {
                        // TODO: i don't like passing key (which came from the stat) to the function on the stat. but it works for now
                        if let Err(err) = stat.save_timeseries("dev_web3_proxy", "global_proxy", self.chain_id, influxdb_client, key).await {
                            error!("unable to save global stat! err={:?}", err);
                        };
                    }

                    for (key, stat) in opt_in_timeseries_buffer.drain() {
                        // TODO: i don't like passing key (which came from the stat) to the function on the stat. but it works for now
                        if let Err(err) = stat.save_timeseries("dev_web3_proxy", "opt_in_proxy", self.chain_id, influxdb_client, key).await {
                            error!("unable to save opt-in stat! err={:?}", err);
                        };
                    }
                }
                x = shutdown_receiver.recv() => {
                    match x {
                        Ok(_) => {
                            info!("stat_loop shutting down");
                            // TODO: call aggregate_stat for all the
                        },
                        Err(err) => error!("stat_loop shutdown receiver err={:?}", err),
                    }
                    break;
                }
            }
        }

        // TODO: dry
        if let Some(db_conn) = self.db_conn.as_ref() {
            info!(
                "saving {} buffered accounting entries",
                accounting_db_buffer.len(),
            );

            for (key, stat) in accounting_db_buffer.drain() {
                if let Err(err) = stat.save_db(self.chain_id, db_conn, key).await {
                    error!(
                        "Unable to save accounting entry while shutting down! err={:?}",
                        err
                    );
                };
            }
        }

        // TODO: dry
        if let Some(influxdb_client) = self.influxdb_client.as_ref() {
            info!(
                "saving {} buffered global stats",
                global_timeseries_buffer.len(),
            );

            for (key, stat) in global_timeseries_buffer.drain() {
                if let Err(err) = stat
                    .save_timeseries(
                        "dev_web3_proxy",
                        "global_proxy",
                        self.chain_id,
                        influxdb_client,
                        key,
                    )
                    .await
                {
                    error!(
                        "Unable to save global stat while shutting down! err={:?}",
                        err
                    );
                };
            }

            info!(
                "saving {} buffered opt-in stats",
                opt_in_timeseries_buffer.len(),
            );

            for (key, stat) in opt_in_timeseries_buffer.drain() {
                if let Err(err) = stat
                    .save_timeseries(
                        "dev_web3_proxy",
                        "opt_in_proxy",
                        self.chain_id,
                        influxdb_client,
                        key,
                    )
                    .await
                {
                    error!(
                        "unable to save opt-in stat while shutting down! err={:?}",
                        err
                    );
                };
            }
        }

        info!("accounting and stat save loop complete");

        Ok(())
    }
}
