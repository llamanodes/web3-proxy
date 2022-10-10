use crate::frontend::authorization::{AuthorizedKey, RequestMetadata};
use chrono::{DateTime, Utc};
use derive_more::From;
use entities::rpc_accounting;
use moka::future::{Cache, CacheBuilder};
use parking_lot::{Mutex, RwLock};
use sea_orm::DatabaseConnection;
use std::sync::atomic::Ordering;
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
    frontend_requests: AtomicU32,
    backend_requests: AtomicU32,
    first_datetime: DateTime<Utc>,
    // TODO: would like to not need a mutex. see how it performs before caring too much
    last_timestamp: Mutex<DateTime<Utc>>,
    first_response_millis: u32,
    sum_response_millis: AtomicU32,
    sum_request_bytes: AtomicU32,
    sum_response_bytes: AtomicU32,
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
        let aggregated_proxy_responses = CacheBuilder::default()
            .time_to_live(Duration::from_secs(period_seconds * 3 / 2))
            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::new());

        let s = Self {
            chain_id,
            db_conn,
            period_seconds,
            aggregated_proxy_responses,
        };

        Arc::new(s)
    }

    pub async fn spawn(
        self: Arc<Self>,
    ) -> anyhow::Result<(flume::Sender<Web3ProxyStat>, JoinHandle<anyhow::Result<()>>)> {
        let (tx, rx) = flume::unbounded::<Web3ProxyStat>();

        // simple future that reads the channel and emits stats
        let f = async move {
            // TODO: select on shutdown handle so we can be sure to save every aggregate!
            while let Ok(x) = rx.recv_async().await {
                trace!(?x, "emitting stat");

                // TODO: increment global stats (in redis? in local cache for prometheus?)

                let clone = self.clone();

                // TODO: batch stats? spawn this?
                // TODO: where can we wait on this handle?
                tokio::spawn(async move { clone.queue_user_stat(x).await });

                // no need to save manually. they save on expire
            }

            // shutting down. force a save
            self.save_user_stats().await?;

            info!("stat emitter exited");

            Ok(())
        };

        let handle = tokio::spawn(f);

        Ok((tx, handle))
    }

    pub async fn queue_user_stat(&self, stat: Web3ProxyStat) -> anyhow::Result<()> {
        match stat {
            Web3ProxyStat::ProxyResponse(x) => {
                // TODO: move this into another function?

                // get the user cache for the current time bucket
                let time_bucket = (x.metadata.datetime.timestamp() as u64) / self.period_seconds;
                let user_cache = self
                    .aggregated_proxy_responses
                    .get_with(time_bucket, async move {
                        CacheBuilder::default()
                            .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::new())
                    })
                    .await;

                let error_response = x.metadata.error_response.load(Ordering::Acquire);

                let key = (x.user_key_id, x.method.clone(), error_response);

                let user_aggregate = user_cache
                    .get_with(key, async move {
                        let last_timestamp = Mutex::new(x.metadata.datetime);

                        let aggregate = ProxyResponseAggregate {
                            first_datetime: x.metadata.datetime,
                            first_response_millis: x
                                .metadata
                                .response_millis
                                .load(Ordering::Acquire),
                            last_timestamp,
                            ..Default::default()
                        };

                        Arc::new(aggregate)
                    })
                    .await;

                todo!();
            }
        }

        Ok(())
    }

    pub async fn save_user_stats(&self) -> anyhow::Result<()> {
        todo!();
    }
}
