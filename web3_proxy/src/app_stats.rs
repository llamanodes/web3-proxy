use crate::frontend::authorization::{AuthorizedKey, RequestMetadata};
use crate::jsonrpc::JsonRpcForwardedResponse;
use chrono::{TimeZone, Utc};
use derive_more::From;
use entities::rpc_accounting;
use hashbrown::HashMap;
use hdrhistogram::{Histogram, RecordError};
use sea_orm::{ActiveModelTrait, DatabaseConnection, DbErr};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::{interval_at, Instant};
use tracing::{error, info};

/// TODO: where should this be defined?
/// TODO: can we use something inside sea_orm instead?
#[derive(Debug)]
pub struct ProxyResponseStat {
    rpc_key_id: u64,
    method: String,
    archive_request: bool,
    request_bytes: u64,
    /// if backend_requests is 0, there was a cache_hit
    backend_requests: u64,
    error_response: bool,
    response_bytes: u64,
    response_millis: u64,
}

pub struct ProxyResponseHistograms {
    request_bytes: Histogram<u64>,
    response_bytes: Histogram<u64>,
    response_millis: Histogram<u64>,
}

impl Default for ProxyResponseHistograms {
    fn default() -> Self {
        // TODO: how many significant figures?
        let request_bytes = Histogram::new(5).expect("creating request_bytes histogram");
        let response_bytes = Histogram::new(5).expect("creating response_bytes histogram");
        let response_millis = Histogram::new(5).expect("creating response_millis histogram");

        Self {
            request_bytes,
            response_bytes,
            response_millis,
        }
    }
}

#[derive(Clone, From, Hash, PartialEq, Eq)]
struct ProxyResponseAggregateKey {
    rpc_key_id: u64,
    method: String,
    error_response: bool,
    archive_request: bool,
}

#[derive(Default)]
pub struct ProxyResponseAggregate {
    frontend_requests: u64,
    backend_requests: u64,
    // TODO: related to backend_requests. get this level of detail out
    // backend_retries: u64,
    // TODO: related to backend_requests. get this level of detail out
    // no_servers: u64,
    cache_misses: u64,
    cache_hits: u64,
    sum_request_bytes: u64,
    sum_response_bytes: u64,
    sum_response_millis: u64,
    histograms: ProxyResponseHistograms,
}

/// A stat that we aggregate and then store in a database.
/// For now there is just one, but I think there might be others later
#[derive(Debug, From)]
pub enum Web3ProxyStat {
    Response(ProxyResponseStat),
}

#[derive(From)]
pub struct StatEmitterSpawn {
    pub stat_sender: flume::Sender<Web3ProxyStat>,
    /// these handles are important and must be allowed to finish
    pub background_handle: JoinHandle<anyhow::Result<()>>,
}

pub struct StatEmitter {
    chain_id: u64,
    db_conn: DatabaseConnection,
    period_seconds: u64,
}

// TODO: impl `+=<ProxyResponseStat>` for ProxyResponseAggregate?
impl ProxyResponseAggregate {
    fn add(&mut self, stat: ProxyResponseStat) -> Result<(), RecordError> {
        // a stat always come from just 1 frontend request
        self.frontend_requests += 1;

        if stat.backend_requests == 0 {
            // no backend request. cache hit!
            self.cache_hits += 1;
        } else {
            // backend requests! cache miss!
            self.cache_misses += 1;

            // a stat might have multiple backend requests
            self.backend_requests += stat.backend_requests;
        }

        self.sum_request_bytes += stat.request_bytes;
        self.sum_response_bytes += stat.response_bytes;
        self.sum_response_millis += stat.response_millis;

        // TODO: use `record_correct`?
        self.histograms.request_bytes.record(stat.request_bytes)?;
        self.histograms
            .response_millis
            .record(stat.response_millis)?;
        self.histograms.response_bytes.record(stat.response_bytes)?;

        Ok(())
    }

    // TODO? help to turn this plus the key into a database model?
    // TODO: take a db transaction instead so that we can batch
    async fn save(
        self,
        chain_id: u64,
        db_conn: &DatabaseConnection,
        key: ProxyResponseAggregateKey,
        period_timestamp: u64,
    ) -> Result<(), DbErr> {
        // this is a lot of variables
        let period_datetime = Utc.timestamp(period_timestamp as i64, 0);

        let request_bytes = &self.histograms.request_bytes;

        let min_request_bytes = request_bytes.min();
        let mean_request_bytes = request_bytes.mean();
        let p50_request_bytes = request_bytes.value_at_quantile(0.50);
        let p90_request_bytes = request_bytes.value_at_quantile(0.90);
        let p99_request_bytes = request_bytes.value_at_quantile(0.99);
        let max_request_bytes = request_bytes.max();

        let response_millis = &self.histograms.response_millis;

        let min_response_millis = response_millis.min();
        let mean_response_millis = response_millis.mean();
        let p50_response_millis = response_millis.value_at_quantile(0.50);
        let p90_response_millis = response_millis.value_at_quantile(0.90);
        let p99_response_millis = response_millis.value_at_quantile(0.99);
        let max_response_millis = response_millis.max();

        let response_bytes = &self.histograms.response_bytes;

        let min_response_bytes = response_bytes.min();
        let mean_response_bytes = response_bytes.mean();
        let p50_response_bytes = response_bytes.value_at_quantile(0.50);
        let p90_response_bytes = response_bytes.value_at_quantile(0.90);
        let p99_response_bytes = response_bytes.value_at_quantile(0.99);
        let max_response_bytes = response_bytes.max();

        let aggregated_stat_model = rpc_accounting::ActiveModel {
            id: sea_orm::NotSet,

            rpc_key_id: sea_orm::Set(key.rpc_key_id),
            chain_id: sea_orm::Set(chain_id),
            method: sea_orm::Set(key.method),
            archive_request: sea_orm::Set(key.archive_request),
            error_response: sea_orm::Set(key.error_response),
            period_datetime: sea_orm::Set(period_datetime),
            frontend_requests: sea_orm::Set(self.frontend_requests),
            backend_requests: sea_orm::Set(self.backend_requests),
            // backend_retries: sea_orm::Set(self.backend_retries),
            // no_servers: sea_orm::Set(self.no_servers),
            cache_misses: sea_orm::Set(self.cache_misses),
            cache_hits: sea_orm::Set(self.cache_hits),

            sum_request_bytes: sea_orm::Set(self.sum_request_bytes),
            min_request_bytes: sea_orm::Set(min_request_bytes),
            mean_request_bytes: sea_orm::Set(mean_request_bytes),
            p50_request_bytes: sea_orm::Set(p50_request_bytes),
            p90_request_bytes: sea_orm::Set(p90_request_bytes),
            p99_request_bytes: sea_orm::Set(p99_request_bytes),
            max_request_bytes: sea_orm::Set(max_request_bytes),

            sum_response_millis: sea_orm::Set(self.sum_response_millis),
            min_response_millis: sea_orm::Set(min_response_millis),
            mean_response_millis: sea_orm::Set(mean_response_millis),
            p50_response_millis: sea_orm::Set(p50_response_millis),
            p90_response_millis: sea_orm::Set(p90_response_millis),
            p99_response_millis: sea_orm::Set(p99_response_millis),
            max_response_millis: sea_orm::Set(max_response_millis),

            sum_response_bytes: sea_orm::Set(self.sum_response_bytes),
            min_response_bytes: sea_orm::Set(min_response_bytes),
            mean_response_bytes: sea_orm::Set(mean_response_bytes),
            p50_response_bytes: sea_orm::Set(p50_response_bytes),
            p90_response_bytes: sea_orm::Set(p90_response_bytes),
            p99_response_bytes: sea_orm::Set(p99_response_bytes),
            max_response_bytes: sea_orm::Set(max_response_bytes),
        };

        aggregated_stat_model.save(db_conn).await?;

        Ok(())
    }
}

impl ProxyResponseStat {
    // TODO: should RequestMetadata be in an arc? or can we handle refs here?
    pub fn new(
        method: String,
        authorized_key: AuthorizedKey,
        metadata: Arc<RequestMetadata>,
        response: &JsonRpcForwardedResponse,
    ) -> Self {
        // TODO: do this without serializing to a string. this is going to slow us down!
        let response_bytes = serde_json::to_string(response)
            .expect("serializing here should always work")
            .len() as u64;

        let archive_request = metadata.archive_request.load(Ordering::Acquire);
        let backend_requests = metadata.backend_requests.load(Ordering::Acquire);
        // let period_seconds = metadata.period_seconds;
        // let period_timestamp =
        //     (metadata.start_datetime.timestamp() as u64) / period_seconds * period_seconds;
        let request_bytes = metadata.request_bytes;
        let error_response = metadata.error_response.load(Ordering::Acquire);

        // TODO: timestamps could get confused by leap seconds. need tokio time instead
        let response_millis = metadata.start_instant.elapsed().as_millis() as u64;

        Self {
            rpc_key_id: authorized_key.rpc_key_id,
            archive_request,
            method,
            backend_requests,
            request_bytes,
            error_response,
            response_bytes,
            response_millis,
        }
    }

    fn key(&self) -> ProxyResponseAggregateKey {
        ProxyResponseAggregateKey {
            rpc_key_id: self.rpc_key_id,
            method: self.method.clone(),
            error_response: self.error_response,
            archive_request: self.archive_request,
        }
    }
}

impl StatEmitter {
    pub fn spawn(
        chain_id: u64,
        db_conn: DatabaseConnection,
        period_seconds: u64,
        shutdown_receiver: broadcast::Receiver<()>,
    ) -> anyhow::Result<StatEmitterSpawn> {
        let (stat_sender, stat_receiver) = flume::unbounded();

        let mut new = Self {
            chain_id,
            db_conn,
            period_seconds,
        };

        let handle =
            tokio::spawn(async move { new.stat_loop(stat_receiver, shutdown_receiver).await });

        Ok((stat_sender, handle).into())
    }

    async fn stat_loop(
        &mut self,
        stat_receiver: flume::Receiver<Web3ProxyStat>,
        mut shutdown_receiver: broadcast::Receiver<()>,
    ) -> anyhow::Result<()> {
        let system_now = SystemTime::now();

        let duration_since_epoch = system_now
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("time machines don't exist");

        // TODO: change period_seconds from u64 to u32
        let current_period = duration_since_epoch
            .checked_div(self.period_seconds as u32)
            .unwrap()
            * self.period_seconds as u32;

        let duration_to_next_period =
            Duration::from_secs(self.period_seconds) - (duration_since_epoch - current_period);

        // start the interval when the next period starts
        let start_instant = Instant::now() + duration_to_next_period;
        let mut interval = interval_at(start_instant, Duration::from_secs(self.period_seconds));

        // loop between different futures to update these mutables
        let mut period_timestamp = current_period.as_secs();
        let mut response_aggregate_map =
            HashMap::<ProxyResponseAggregateKey, ProxyResponseAggregate>::new();

        loop {
            tokio::select! {
                stat = stat_receiver.recv_async() => {
                    match stat? {
                        Web3ProxyStat::Response(stat) => {
                            let key = stat.key();

                            // TODO: does hashmap have get_or_insert?
                            if ! response_aggregate_map.contains_key(&key) {
                                response_aggregate_map.insert(key.clone(), Default::default());
                            };

                            if let Some(value) = response_aggregate_map.get_mut(&key) {
                                if let Err(err) = value.add(stat) {
                                    error!(?err, "unable to aggregate stats!");
                                };
                            } else {
                                unimplemented!();
                            }
                        }
                    }
                }
                _ = interval.tick() => {
                    // save all the aggregated stats
                    // TODO: batch these saves
                    for (key, aggregate) in response_aggregate_map.drain() {
                        if let Err(err) = aggregate.save(self.chain_id, &self.db_conn, key, period_timestamp).await {
                            error!(?err, "Unable to save stat while shutting down!");
                        };
                    }
                    // advance to the next period
                    // TODO: is this safe? what if there is drift?
                    period_timestamp += self.period_seconds;
                }
                x = shutdown_receiver.recv() => {
                    match x {
                        Ok(_) => {
                            info!("aggregate stat_loop shutting down");
                            // TODO: call aggregate_stat for all the
                        },
                        Err(err) => error!(?err, "shutdown receiver"),
                    }
                    break;
                }
            }
        }

        info!("saving {} stats", response_aggregate_map.len());

        for (key, aggregate) in response_aggregate_map.drain() {
            if let Err(err) = aggregate
                .save(self.chain_id, &self.db_conn, key, period_timestamp)
                .await
            {
                error!(?err, "Unable to save stat while shutting down!");
            };
        }

        info!("aggregated stat_loop shut down");

        Ok(())
    }
}
