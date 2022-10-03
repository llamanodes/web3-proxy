use chrono::{DateTime, Utc};
use derive_more::From;
use influxdb::Client;
use influxdb::InfluxDbWriteable;
use tokio::task::JoinHandle;
use tracing::{error, info, trace};

use crate::frontend::authorization::AuthorizedRequest;

#[derive(Debug)]
pub enum ProxyResponseType {
    CacheHit,
    CacheMiss,
    Error,
}

impl From<ProxyResponseType> for influxdb::Type {
    fn from(x: ProxyResponseType) -> Self {
        match x {
            ProxyResponseType::CacheHit => "cache_hit".into(),
            ProxyResponseType::CacheMiss => "cache_miss".into(),
            ProxyResponseType::Error => "error".into(),
        }
    }
}

/// TODO: where should this be defined?
/// TODO: what should be fields and what should be tags. count is always 1 which feels wrong
#[derive(Debug, InfluxDbWriteable)]
pub struct ProxyResponseStat {
    time: DateTime<Utc>,
    count: u32,
    #[influxdb(tag)]
    method: String,
    #[influxdb(tag)]
    response_type: ProxyResponseType,
    #[influxdb(tag)]
    who: String,
}

impl ProxyResponseStat {
    pub fn new(method: String, response_type: ProxyResponseType, who: &AuthorizedRequest) -> Self {
        Self {
            time: Utc::now(),
            count: 1,
            method,
            response_type,
            who: who.to_string(),
        }
    }
}

#[derive(Debug, From)]
pub enum Web3ProxyStat {
    ProxyResponse(ProxyResponseStat),
}

impl Web3ProxyStat {
    fn into_query(self) -> influxdb::WriteQuery {
        match self {
            Self::ProxyResponse(x) => x.into_query("proxy_response"),
        }
    }
}

pub struct StatEmitter;

impl StatEmitter {
    pub fn spawn(
        influxdb_url: String,
        influxdb_name: String,
        http_client: Option<reqwest::Client>,
    ) -> (flume::Sender<Web3ProxyStat>, JoinHandle<anyhow::Result<()>>) {
        let (tx, rx) = flume::unbounded::<Web3ProxyStat>();

        let client = Client::new(influxdb_url, influxdb_name);

        // use an existing http client
        let client = if let Some(http_client) = http_client {
            client.with_http_client(http_client)
        } else {
            client
        };

        let f = async move {
            while let Ok(x) = rx.recv_async().await {
                let x = x.into_query();

                trace!(?x, "emitting stat");

                if let Err(err) = client.query(x).await {
                    error!(?err, "failed writing stat");
                    // TODO: now what?
                }
            }

            info!("stat emitter exited");

            Ok(())
        };

        let handle = tokio::spawn(f);

        (tx, handle)
    }
}
