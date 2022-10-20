use crate::frontend::authorization::{AuthorizedKey, RequestMetadata};
use crate::jsonrpc::JsonRpcForwardedResponse;
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
use tokio::sync::{broadcast, Mutex as AsyncMutex};
use tokio::task::JoinHandle;
use tracing::{error, info, trace};

/// TODO: where should this be defined?
/// TODO: can we use something inside sea_orm instead?
#[derive(Debug)]
pub struct ProxyResponseStat {
    user_key_id: u64,
    method: String,
    period_seconds: u64,
    period_timestamp: u64,
    request_bytes: u64,
    /// if this is 0, there was a cache_hit
    backend_requests: u32,
    error_response: bool,
    response_bytes: u64,
    response_millis: u64,
}

pub type TimeBucketTimestamp = u64;

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
    no_servers: AtomicU32,
    cache_misses: AtomicU32,
    cache_hits: AtomicU32,
    sum_request_bytes: AtomicU64,
    sum_response_bytes: AtomicU64,
    sum_response_millis: AtomicU64,
    histograms: AsyncMutex<ProxyResponseHistograms>,
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

        let backend_requests = metadata.backend_requests.load(Ordering::Acquire);
        let period_seconds = metadata.period_seconds;
        let period_timestamp =
            (metadata.datetime.timestamp() as u64) / period_seconds * period_seconds;
        let request_bytes = metadata.request_bytes;
        let error_response = metadata.error_response.load(Ordering::Acquire);

        let response_millis =
            (Utc::now().timestamp_millis() - metadata.datetime.timestamp_millis()) as u64;

        Self {
            user_key_id: authorized_key.user_key_id,
            method,
            backend_requests,
            period_seconds,
            period_timestamp,
            request_bytes,
            error_response,
            response_bytes,
            response_millis,
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
        shutdown_receiver: broadcast::Receiver<()>,
    ) -> anyhow::Result<(
        flume::Sender<Web3ProxyStat>,
        JoinHandle<anyhow::Result<()>>,
        JoinHandle<anyhow::Result<()>>,
    )> {
        let (aggregate_tx, aggregate_rx) = flume::unbounded::<Web3ProxyStat>();

        // TODO: join and flatten these handles
        let aggregate_handle = tokio::spawn(
            self.clone()
                .aggregate_stats_loop(aggregate_rx, shutdown_receiver),
        );
        let save_handle = tokio::spawn(self.save_stats_loop());

        Ok((aggregate_tx, aggregate_handle, save_handle))
    }

    /// simple future that reads the channel and aggregates stats in a local cache.
    async fn aggregate_stats_loop(
        self: Arc<Self>,
        aggregate_rx: flume::Receiver<Web3ProxyStat>,
        mut shutdown_receiver: broadcast::Receiver<()>,
    ) -> anyhow::Result<()> {
        // TODO: select on shutdown handle so we can be sure to save every aggregate!
        tokio::select! {
            x = aggregate_rx.recv_async() => {
                match x {
                    Ok(x) => {
                        trace!(?x, "aggregating stat");

                        // TODO: increment global stats (in redis? in local cache for prometheus?)

                        // TODO: batch stats?
                        // TODO: where can we wait on this handle?
                        let clone = self.clone();
                        tokio::spawn(async move { clone.aggregate_stat(x).await });
                    },
                    Err(err) => {
                        error!(?err, "aggregate_rx");
                    }
                }
            }
            x = shutdown_receiver.recv() => {
                match x {
                    Ok(_) => info!("aggregate stats loop shutting down"),
                    Err(err) => error!(?err, "shutdown receiver"),
                }
            }
        }

        // shutting down. force a save of any pending stats
        // we do not use invalidate_all because that is done on a background thread
        for (key, _) in self.aggregated_proxy_responses.into_iter() {
            self.aggregated_proxy_responses.invalidate(&key).await;
        }

        info!("aggregate stats loop finished");

        Ok(())
    }

    async fn save_stats_loop(self: Arc<Self>) -> anyhow::Result<()> {
        while let Ok(x) = self.save_rx.recv_async().await {
            // TODO: batch these
            for (k, v) in x.into_iter() {
                // TODO: this is a lot of variables
                let period_datetime = Utc.timestamp(v.period_timestamp as i64, 0);
                let frontend_requests = v.frontend_requests.load(Ordering::Acquire);
                let backend_requests = v.backend_requests.load(Ordering::Acquire);
                let backend_retries = v.backend_retries.load(Ordering::Acquire);
                let no_servers = v.no_servers.load(Ordering::Acquire);
                let cache_misses = v.cache_misses.load(Ordering::Acquire);
                let cache_hits = v.cache_hits.load(Ordering::Acquire);
                let sum_request_bytes = v.sum_request_bytes.load(Ordering::Acquire);
                let sum_response_millis = v.sum_response_millis.load(Ordering::Acquire);
                let sum_response_bytes = v.sum_response_bytes.load(Ordering::Acquire);

                let histograms = v.histograms.lock().await;

                let request_bytes = &histograms.request_bytes;

                let min_request_bytes = request_bytes.min();
                let mean_request_bytes = request_bytes.mean();
                let p50_request_bytes = request_bytes.value_at_quantile(0.50);
                let p90_request_bytes = request_bytes.value_at_quantile(0.90);
                let p99_request_bytes = request_bytes.value_at_quantile(0.99);
                let max_request_bytes = request_bytes.max();

                let response_millis = &histograms.response_millis;

                let min_response_millis = response_millis.min();
                let mean_response_millis = response_millis.mean();
                let p50_response_millis = response_millis.value_at_quantile(0.50);
                let p90_response_millis = response_millis.value_at_quantile(0.90);
                let p99_response_millis = response_millis.value_at_quantile(0.99);
                let max_response_millis = response_millis.max();

                let response_bytes = &histograms.response_bytes;

                let min_response_bytes = response_bytes.min();
                let mean_response_bytes = response_bytes.mean();
                let p50_response_bytes = response_bytes.value_at_quantile(0.50);
                let p90_response_bytes = response_bytes.value_at_quantile(0.90);
                let p99_response_bytes = response_bytes.value_at_quantile(0.99);
                let max_response_bytes = response_bytes.max();

                drop(histograms);

                let stat = rpc_accounting::ActiveModel {
                    id: sea_orm::NotSet,

                    user_key_id: sea_orm::Set(k.user_key_id),
                    chain_id: sea_orm::Set(self.chain_id),
                    method: sea_orm::Set(k.method.clone()),
                    error_response: sea_orm::Set(k.error_response),
                    period_datetime: sea_orm::Set(period_datetime),
                    frontend_requests: sea_orm::Set(frontend_requests),
                    backend_requests: sea_orm::Set(backend_requests),
                    backend_retries: sea_orm::Set(backend_retries),
                    no_servers: sea_orm::Set(no_servers),
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
            Web3ProxyStat::ProxyResponse(stat) => {
                // TODO: move this whole closure to another function?

                debug_assert_eq!(stat.period_seconds, self.period_seconds);

                // get the user cache for the current period
                let user_cache = self
                    .aggregated_proxy_responses
                    .get_with(stat.period_timestamp, async move {
                        CacheBuilder::default()
                            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::new())
                    })
                    .await;

                let key = (stat.user_key_id, stat.method, stat.error_response).into();

                let user_aggregate = user_cache
                    .get_with(key, async move {
                        let histograms = ProxyResponseHistograms::default();

                        let aggregate = ProxyResponseAggregate {
                            period_timestamp: stat.period_timestamp,
                            // start most things at 0 because we add outside this getter
                            frontend_requests: 0.into(),
                            backend_requests: 0.into(),
                            backend_retries: 0.into(),
                            no_servers: 0.into(),
                            cache_misses: 0.into(),
                            cache_hits: 0.into(),
                            sum_request_bytes: 0.into(),
                            sum_response_bytes: 0.into(),
                            sum_response_millis: 0.into(),
                            histograms: AsyncMutex::new(histograms),
                        };

                        Arc::new(aggregate)
                    })
                    .await;

                // a stat always come from just 1 frontend request
                user_aggregate
                    .frontend_requests
                    .fetch_add(1, Ordering::Acquire);

                if stat.backend_requests == 0 {
                    // no backend request. cache hit!
                    user_aggregate.cache_hits.fetch_add(1, Ordering::Acquire);
                } else {
                    // backend requests! cache miss!
                    user_aggregate.cache_misses.fetch_add(1, Ordering::Acquire);

                    // a stat might have multiple backend requests
                    user_aggregate
                        .backend_requests
                        .fetch_add(stat.backend_requests, Ordering::Acquire);
                }

                user_aggregate
                    .sum_request_bytes
                    .fetch_add(stat.request_bytes, Ordering::Release);

                user_aggregate
                    .sum_response_bytes
                    .fetch_add(stat.response_bytes, Ordering::Release);

                user_aggregate
                    .sum_response_millis
                    .fetch_add(stat.response_millis, Ordering::Release);

                {
                    let mut histograms = user_aggregate.histograms.lock().await;

                    // TODO: use `record_correct`?
                    histograms.request_bytes.record(stat.request_bytes)?;
                    histograms.response_millis.record(stat.response_millis)?;
                    histograms.response_bytes.record(stat.response_bytes)?;
                }
            }
        }

        Ok(())
    }
}
