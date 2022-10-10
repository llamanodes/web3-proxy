use crate::frontend::authorization::{AuthorizedKey, RequestMetadata};
use anyhow::Context;
use chrono::{TimeZone, Utc};
use derive_more::From;
use entities::rpc_accounting;
use moka::future::{Cache, CacheBuilder};
use sea_orm::{ActiveModelTrait, DatabaseConnection};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::{
    sync::atomic::{AtomicU32, AtomicU64},
    time::Duration,
};
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

// TODO: impl From for our database model
#[derive(Default)]
pub struct ProxyResponseAggregate {
    // user_key_id: u64,
    // method: String,
    // error_response: bool,
    first_timestamp: u64,
    frontend_requests: AtomicU32,
    backend_requests: AtomicU32,
    last_timestamp: AtomicU64,
    first_response_millis: u32,
    sum_response_millis: AtomicU32,
    sum_request_bytes: AtomicUsize,
    sum_response_bytes: AtomicUsize,
}

/// key is the (user_key_id, method, error_response)
pub type UserProxyResponseCache = Cache<
    (u64, String, bool),
    Arc<ProxyResponseAggregate>,
    hashbrown::hash_map::DefaultHashBuilder,
>;
/// key is the "time bucket" (timestamp / period)
pub type TimeProxyResponseCache =
    Cache<u64, UserProxyResponseCache, hashbrown::hash_map::DefaultHashBuilder>;

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

        let aggregated_proxy_responses = CacheBuilder::default()
            .time_to_live(Duration::from_secs(period_seconds * 3 / 2))
            .eviction_listener_with_queued_delivery_mode(move |k, v, r| {
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

        // simple future that reads the channel and emits stats
        let aggregate_f = {
            let aggregated_proxy_responses = self.aggregated_proxy_responses.clone();
            let clone = self.clone();
            async move {
                // TODO: select on shutdown handle so we can be sure to save every aggregate!
                while let Ok(x) = aggregate_rx.recv_async().await {
                    trace!(?x, "aggregating stat");

                    // TODO: increment global stats (in redis? in local cache for prometheus?)

                    // TODO: batch stats? spawn this?
                    // TODO: where can we wait on this handle?
                    let clone = clone.clone();
                    tokio::spawn(async move { clone.aggregate_stat(x).await });

                    // no need to save manually. they save on expire
                }

                // shutting down. force a save
                // TODO: this is handled by a background thread! we need to make sure the thread survives long enough to do its work!
                aggregated_proxy_responses.invalidate_all();

                info!("stat aggregator exited");

                Ok(())
            }
        };

        let save_f = {
            let db_conn = self.db_conn.clone();
            let save_rx = self.save_rx.clone();
            let chain_id = self.chain_id;
            async move {
                while let Ok(x) = save_rx.recv_async().await {
                    // TODO: batch these
                    for (k, v) in x.into_iter() {
                        // TODO: try_unwrap()?
                        let (user_key_id, method, error_response) = k.as_ref();

                        info!(?user_key_id, ?method, ?error_response, "saving");

                        let first_timestamp = Utc.timestamp(v.first_timestamp as i64, 0);
                        let frontend_requests = v.frontend_requests.load(Ordering::Acquire);
                        let backend_requests = v.backend_requests.load(Ordering::Acquire);
                        let first_response_millis = v.first_response_millis;
                        let sum_request_bytes = v.sum_request_bytes.load(Ordering::Acquire);
                        let sum_response_millis = v.sum_response_millis.load(Ordering::Acquire);
                        let sum_response_bytes = v.sum_response_bytes.load(Ordering::Acquire);

                        let stat = rpc_accounting::ActiveModel {
                            user_key_id: sea_orm::Set(*user_key_id),
                            chain_id: sea_orm::Set(chain_id),
                            method: sea_orm::Set(method.clone()),
                            error_response: sea_orm::Set(*error_response),
                            first_timestamp: sea_orm::Set(first_timestamp),
                            frontend_requests: sea_orm::Set(frontend_requests)
                            backend_requests: sea_orm::Set(backend_requests),
                            first_query_millis: sea_orm::Set(first_query_millis),
                            sum_request_bytes: sea_orm::Set(sum_request_bytes),
                            sum_response_millis: sea_orm::Set(sum_response_millis),
                            sum_response_bytes: sea_orm::Set(sum_response_bytes),
                            ..Default::default()
                        };

                        // TODO: if this fails, rever adding the user, too
                        if let Err(err) = stat
                            .save(&db_conn)
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
        };

        // TODO: join and flatten these handles
        let aggregate_handle = tokio::spawn(aggregate_f);
        let save_handle = tokio::spawn(save_f);

        Ok((aggregate_tx, aggregate_handle, save_handle))
    }

    pub async fn aggregate_stat(&self, stat: Web3ProxyStat) -> anyhow::Result<()> {
        trace!(?stat, "aggregating");
        match stat {
            Web3ProxyStat::ProxyResponse(x) => {
                // TODO: move this into another function?

                // get the user cache for the current time bucket
                let time_bucket = x.metadata.timestamp / self.period_seconds;
                let user_cache = self
                    .aggregated_proxy_responses
                    .get_with(time_bucket, async move {
                        CacheBuilder::default()
                            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::new())
                    })
                    .await;

                let error_response = x.metadata.error_response.load(Ordering::Acquire);

                let key = (x.user_key_id, x.method, error_response);

                let timestamp = x.metadata.timestamp;
                let response_millis = x.metadata.response_millis.load(Ordering::Acquire);

                let user_aggregate = user_cache
                    .get_with(key, async move {
                        let last_timestamp = timestamp.into();

                        let aggregate = ProxyResponseAggregate {
                            first_timestamp: timestamp,
                            first_response_millis: response_millis,
                            last_timestamp,
                            ..Default::default()
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
                user_aggregate
                    .sum_request_bytes
                    .fetch_add(request_bytes, Ordering::Release);

                let response_bytes = x.metadata.response_bytes.load(Ordering::Acquire);
                user_aggregate
                    .sum_response_bytes
                    .fetch_add(response_bytes, Ordering::Release);

                user_aggregate
                    .sum_response_millis
                    .fetch_add(response_millis, Ordering::Release);

                user_aggregate
                    .last_timestamp
                    .fetch_max(x.metadata.timestamp, Ordering::Release);
            }
        }

        Ok(())
    }
}
