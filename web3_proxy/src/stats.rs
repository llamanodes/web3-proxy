use chrono::{DateTime, Utc};
use influxdb::InfluxDbWriteable;
use influxdb::{Client, Query, ReadQuery, Timestamp};
use tokio::task::JoinHandle;
use tracing::{error, info};

/// TODO: replace this example stat with our own
#[derive(InfluxDbWriteable)]
pub struct WeatherReading {
    time: DateTime<Utc>,
    humidity: i32,
    #[influxdb(tag)]
    wind_direction: String,
}

pub enum Web3ProxyStat {
    WeatherReading(WeatherReading),
}

impl Web3ProxyStat {
    fn into_query(self) -> influxdb::WriteQuery {
        match self {
            Self::WeatherReading(x) => x.into_query("weather"),
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
                if let Err(err) = client.query(x.into_query()).await {
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
