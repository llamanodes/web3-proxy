use anyhow::Context;
use derive_more::From;
use redis_rate_limiter::{redis, RedisConnection};
use std::fmt::Display;
use tokio::task::JoinHandle;
use tracing::{debug, error, info};

use crate::frontend::authorization::AuthorizedRequest;

#[derive(Debug)]
pub enum ProxyResponseType {
    CacheHit,
    CacheMiss,
    Error,
}

impl Display for ProxyResponseType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProxyResponseType::CacheHit => f.write_str("cache_hit"),
            ProxyResponseType::CacheMiss => f.write_str("cache_miss"),
            ProxyResponseType::Error => f.write_str("error"),
        }
    }
}

/// TODO: where should this be defined?
#[derive(Debug)]
pub struct ProxyResponseStat(String);

/// A very basic stat that we store in redis.
/// This probably belongs in a true time series database like influxdb, but client
impl ProxyResponseStat {
    pub fn new(method: String, response_type: ProxyResponseType, who: &AuthorizedRequest) -> Self {
        // TODO: what order?
        // TODO: app specific prefix. need at least the chain id
        let redis_key = format!("proxy_response:{}:{}:{}", method, response_type, who);

        Self(redis_key)
    }
}

#[derive(Debug, From)]
pub enum Web3ProxyStat {
    ProxyResponse(ProxyResponseStat),
}

impl Web3ProxyStat {
    fn into_redis_key(self, chain_id: u64) -> String {
        match self {
            Self::ProxyResponse(x) => format!("{}:{}", x.0, chain_id),
        }
    }
}

pub struct StatEmitter;

impl StatEmitter {
    pub async fn spawn(
        chain_id: u64,
        mut redis_conn: RedisConnection,
    ) -> anyhow::Result<(flume::Sender<Web3ProxyStat>, JoinHandle<anyhow::Result<()>>)> {
        let (tx, rx) = flume::unbounded::<Web3ProxyStat>();

        // simple future that reads the channel and emits stats
        let f = async move {
            while let Ok(x) = rx.recv_async().await {
                // TODO: batch stats? spawn this?

                let x = x.into_redis_key(chain_id);

                // TODO: this is too loud. just doing it for dev
                debug!(?x, "emitting stat");

                // TODO: do this without the pipe?
                if let Err(err) = redis::pipe()
                    .incr(&x, 1)
                    .query_async::<_, ()>(&mut redis_conn)
                    .await
                    .context("incrementing stat")
                {
                    error!(?err, "emitting stat")
                }
            }

            info!("stat emitter exited");

            Ok(())
        };

        let handle = tokio::spawn(f);

        Ok((tx, handle))
    }
}
