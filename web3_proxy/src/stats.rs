use crate::frontend::authorization::{AuthorizedKey, RequestMetadata};
use anyhow::Context;
use chrono::{TimeZone, Utc};
use derive_more::From;
use entities::rpc_accounting;
use hdrhistogram::Histogram;
use moka::future::{Cache, CacheBuilder};
use sea_orm::{ActiveModelTrait, DatabaseConnection};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::{sync::atomic::AtomicU32, time::Duration};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;
use tracing::{error, info, trace};

/// TODO: where should this be defined?
/// TODO: can we use something inside sea_orm instead?
#[derive(Debug)]
pub struct ProxyResponseStat {
    user_key_id: u64,
    method: String,
    metadata: RequestMetadata,
}

pub type TimeBucketTimestamp = u64;

// TODO: impl From for our database model
pub struct ProxyResponseAggregate {
    // these are the key
    // user_key_id: u64,
    // method: String,
    // error_response: bool,
    // TODO: this is the grandparent key. get it from there somehow
    period_timestamp: u64,
    frontend_requests: AtomicU32,
    backend_requests: AtomicU32,
    backend_retries: AtomicU32,
    cache_misses: AtomicU32,
    cache_hits: AtomicU32,
    request_bytes: AsyncMutex<Histogram<u64>>,
    sum_request_bytes: AtomicU64,
    response_bytes: AsyncMutex<Histogram<u64>>,
    sum_response_bytes: AtomicU64,
    response_millis: AsyncMutex<Histogram<u64>>,
    sum_response_millis: AtomicU64,
}

#[derive(Clone, Debug, From, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct UserProxyResponseKey {
    user_key_id: u64,
    method: String,
    error_response: bool,
}

/// key is the (user_key_id, method, error_response)
pub type UserProxyResponseCache = Cache<
    UserProxyResponseKey,
    Arc<ProxyResponseAggregate>,
    hashbrown::hash_map::DefaultHashBuilder,
>;
/// key is the "time bucket's timestamp" (timestamp / period * period)
pub type TimeProxyResponseCache =
    Cache<TimeBucketTimestamp, UserProxyResponseCache, hashbrown::hash_map::DefaultHashBuilder>;

pub struct StatEmitter {
    chain_id: u64,
    db_conn: DatabaseConnection,
    period_seconds: u64,
    /// the outer cache has a TTL and a handler for expiration
    aggregated_proxy_responses: TimeProxyResponseCache,
    save_rx: flume::Receiver<UserProxyResponseCache>,
}

/// A stat that we aggregate and then store in a database.
#[derive(Debug, From)]
pub enum Web3ProxyStat {
    ProxyResponse(ProxyResponseStat),
}

impl ProxyResponseStat {
    // TODO: should RequestMetadata be in an arc? or can we handle refs here?
    pub fn new(method: String, authorized_key: AuthorizedKey, metadata: RequestMetadata) -> Self {
        Self {
            user_key_id: authorized_key.user_key_id,
            method,
            metadata,
        }
    }
}

impl StatEmitter {
    pub fn new(chain_id: u64, db_conn: DatabaseConnection, period_seconds: u64) -> Arc<Self> {
        let (save_tx, save_rx) = flume::unbounded();

        // this needs to be long enough that there are definitely no outstanding queries
        // TODO: what should the "safe" multiplier be? what if something is late?
        let ttl_seconds = period_seconds * 3;

        let aggregated_proxy_responses = CacheBuilder::default()
            .time_to_live(Duration::from_secs(ttl_seconds))
            .eviction_listener_with_queued_delivery_mode(move |_, v, _| {
                // this function must not panic!
                if let Err(err) = save_tx.send(v) {
                    error!(?err, "unable to save. sender closed!");
                }
            })
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::new());

        let s = Self {
            chain_id,
            db_conn,
            period_seconds,
            aggregated_proxy_responses,
            save_rx,
        };

        Arc::new(s)
    }

    pub async fn spawn(
        self: Arc<Self>,
    ) -> anyhow::Result<(
        flume::Sender<Web3ProxyStat>,
        JoinHandle<anyhow::Result<()>>,
        JoinHandle<anyhow::Result<()>>,
    )> {
        let (aggregate_tx, aggregate_rx) = flume::unbounded::<Web3ProxyStat>();

        // TODO: join and flatten these handles
        let aggregate_handle = tokio::spawn(self.clone().aggregate_stats_loop(aggregate_rx));
        let save_handle = { tokio::spawn(self.save_stats_loop()) };

        Ok((aggregate_tx, aggregate_handle, save_handle))
    }

    /// simple future that reads the channel and aggregates stats in a local cache.
    async fn aggregate_stats_loop(
        self: Arc<Self>,
        aggregate_rx: flume::Receiver<Web3ProxyStat>,
    ) -> anyhow::Result<()> {
        // TODO: select on shutdown handle so we can be sure to save every aggregate!
        while let Ok(x) = aggregate_rx.recv_async().await {
            trace!(?x, "aggregating stat");

            // TODO: increment global stats (in redis? in local cache for prometheus?)

            // TODO: batch stats? spawn this?
            // TODO: where can we wait on this handle?
            let clone = self.clone();
            tokio::spawn(async move { clone.aggregate_stat(x).await });

            // no need to save manually. they save on expire
        }

        // shutting down. force a save
        // we do not use invalidate_all because that is done on a background thread
        for (key, _) in self.aggregated_proxy_responses.into_iter() {
            self.aggregated_proxy_responses.invalidate(&key).await;
        }

        info!("stat aggregator exited");

        Ok(())
    }

    async fn save_stats_loop(self: Arc<Self>) -> anyhow::Result<()> {
        while let Ok(x) = self.save_rx.recv_async().await {
            // TODO: batch these
            for (k, v) in x.into_iter() {
                info!(?k, "saving");

                let period_datetime = Utc.timestamp(v.period_timestamp as i64, 0);
                let frontend_requests = v.frontend_requests.load(Ordering::Acquire);
                let backend_requests = v.backend_requests.load(Ordering::Acquire);
                let backend_retries = v.backend_retries.load(Ordering::Acquire);
                let cache_misses = v.cache_misses.load(Ordering::Acquire);
                let cache_hits = v.cache_hits.load(Ordering::Acquire);

                let request_bytes = v.request_bytes.lock().await;

                let sum_request_bytes = v.sum_request_bytes.load(Ordering::Acquire);
                let min_request_bytes = request_bytes.min();
                let mean_request_bytes = request_bytes.mean();
                let p50_request_bytes = request_bytes.value_at_quantile(0.50);
                let p90_request_bytes = request_bytes.value_at_quantile(0.90);
                let p99_request_bytes = request_bytes.value_at_quantile(0.99);
                let max_request_bytes = request_bytes.max();

                drop(request_bytes);

                let response_millis = v.response_millis.lock().await;

                let sum_response_millis = v.sum_response_millis.load(Ordering::Acquire);
                let min_response_millis = response_millis.min();
                let mean_response_millis = response_millis.mean();
                let p50_response_millis = response_millis.value_at_quantile(0.50);
                let p90_response_millis = response_millis.value_at_quantile(0.90);
                let p99_response_millis = response_millis.value_at_quantile(0.99);
                let max_response_millis = response_millis.max();

                drop(response_millis);

                let response_bytes = v.response_bytes.lock().await;

                let sum_response_bytes = v.sum_response_bytes.load(Ordering::Acquire);
                let min_response_bytes = response_bytes.min();
                let mean_response_bytes = response_bytes.mean();
                let p50_response_bytes = response_bytes.value_at_quantile(0.50);
                let p90_response_bytes = response_bytes.value_at_quantile(0.90);
                let p99_response_bytes = response_bytes.value_at_quantile(0.99);
                let max_response_bytes = response_bytes.max();

                drop(response_bytes);

                let stat = rpc_accounting::ActiveModel {
                    user_key_id: sea_orm::Set(k.user_key_id),
                    chain_id: sea_orm::Set(self.chain_id),
                    method: sea_orm::Set(k.method.clone()),
                    error_response: sea_orm::Set(k.error_response),
                    period_datetime: sea_orm::Set(period_datetime),
                    frontend_requests: sea_orm::Set(frontend_requests),
                    backend_requests: sea_orm::Set(backend_requests),
                    backend_retries: sea_orm::Set(backend_retries),
                    cache_misses: sea_orm::Set(cache_misses),
                    cache_hits: sea_orm::Set(cache_hits),

                    sum_request_bytes: sea_orm::Set(sum_request_bytes),
                    min_request_bytes: sea_orm::Set(min_request_bytes),
                    mean_request_bytes: sea_orm::Set(mean_request_bytes),
                    p50_request_bytes: sea_orm::Set(p50_request_bytes),
                    p90_request_bytes: sea_orm::Set(p90_request_bytes),
                    p99_request_bytes: sea_orm::Set(p99_request_bytes),
                    max_request_bytes: sea_orm::Set(max_request_bytes),

                    sum_response_millis: sea_orm::Set(sum_response_millis),
                    min_response_millis: sea_orm::Set(min_response_millis),
                    mean_response_millis: sea_orm::Set(mean_response_millis),
                    p50_response_millis: sea_orm::Set(p50_response_millis),
                    p90_response_millis: sea_orm::Set(p90_response_millis),
                    p99_response_millis: sea_orm::Set(p99_response_millis),
                    max_response_millis: sea_orm::Set(max_response_millis),

                    sum_response_bytes: sea_orm::Set(sum_response_bytes),
                    min_response_bytes: sea_orm::Set(min_response_bytes),
                    mean_response_bytes: sea_orm::Set(mean_response_bytes),
                    p50_response_bytes: sea_orm::Set(p50_response_bytes),
                    p90_response_bytes: sea_orm::Set(p90_response_bytes),
                    p99_response_bytes: sea_orm::Set(p99_response_bytes),
                    max_response_bytes: sea_orm::Set(max_response_bytes),
                    ..Default::default()
                };

                // TODO: if this fails, rever adding the user, too
                if let Err(err) = stat
                    .save(&self.db_conn)
                    .await
                    .context("Saving rpc_accounting stat")
                {
                    error!(?err, "unable to save aggregated stats");
                }
            }
        }

        info!("stat saver exited");

        Ok(())
    }

    pub async fn aggregate_stat(&self, stat: Web3ProxyStat) -> anyhow::Result<()> {
        trace!(?stat, "aggregating");
        match stat {
            Web3ProxyStat::ProxyResponse(x) => {
                // TODO: move this whole closure to another function?
                // TODO: move period calculation into another function?
                let period_timestamp =
                    x.metadata.timestamp / self.period_seconds * self.period_seconds;

                // get the user cache for the current period
                let user_cache = self
                    .aggregated_proxy_responses
                    .get_with(period_timestamp, async move {
                        CacheBuilder::default()
                            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::new())
                    })
                    .await;

                let error_response = x.metadata.error_response.load(Ordering::Acquire);

                let key = (x.user_key_id, x.method, error_response).into();

                let user_aggregate = user_cache
                    .get_with(key, async move {
                        let aggregate = ProxyResponseAggregate {
                            period_timestamp,
                            // start most things at 0 because we add outside this getter
                            frontend_requests: 0.into(),
                            backend_requests: 0.into(),
                            backend_retries: 0.into(),
                            cache_misses: 0.into(),
                            cache_hits: 0.into(),
                            // TODO: how many significant figures?
                            request_bytes: AsyncMutex::new(
                                Histogram::new(5).expect("creating request_bytes histogram"),
                            ),
                            sum_request_bytes: 0.into(),
                            response_bytes: AsyncMutex::new(
                                Histogram::new(5).expect("creating response_bytes histogram"),
                            ),
                            sum_response_bytes: 0.into(),
                            // TODO: new_with_max here?
                            response_millis: AsyncMutex::new(
                                Histogram::new(5).expect("creating response_millis histogram"),
                            ),
                            sum_response_millis: 0.into(),
                        };

                        Arc::new(aggregate)
                    })
                    .await;

                user_aggregate
                    .backend_requests
                    .fetch_add(1, Ordering::Acquire);

                user_aggregate
                    .frontend_requests
                    .fetch_add(1, Ordering::Acquire);

                let request_bytes = x.metadata.request_bytes.load(Ordering::Acquire);

                let mut request_bytes_histogram = user_aggregate.request_bytes.lock().await;

                // TODO: record_correct?
                request_bytes_histogram.record(request_bytes)?;

                drop(request_bytes_histogram);

                user_aggregate
                    .sum_request_bytes
                    .fetch_add(request_bytes, Ordering::Release);

                let response_bytes = x.metadata.response_bytes.load(Ordering::Acquire);

                let mut response_bytes_histogram = user_aggregate.response_bytes.lock().await;

                // TODO: record_correct?
                response_bytes_histogram.record(response_bytes)?;

                drop(response_bytes_histogram);

                user_aggregate
                    .sum_response_bytes
                    .fetch_add(response_bytes, Ordering::Release);

                let response_millis = x.metadata.response_millis.load(Ordering::Acquire);

                let mut response_millis_histogram = user_aggregate.response_millis.lock().await;

                // TODO: record_correct?
                response_millis_histogram.record(response_millis)?;

                drop(response_millis_histogram);

                user_aggregate
                    .sum_response_millis
                    .fetch_add(response_millis, Ordering::Release);
            }
        }

        Ok(())
    }
}
