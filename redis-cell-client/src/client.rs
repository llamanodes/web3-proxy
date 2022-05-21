use std::time;
use std::time::{Duration, SystemTime};

use redis::aio::MultiplexedConnection;
use redis::AsyncCommands;

const KEY_PREFIX: &str = "rate-limit";

#[derive(Clone)]
pub struct RedisCellClient {
    conn: MultiplexedConnection,
}

impl RedisCellClient {
    pub async fn open(redis_address: &str) -> anyhow::Result<Self> {
        let client = redis::Client::open(redis_address)?;

        let conn = client.get_multiplexed_tokio_connection().await?;

        Ok(Self { conn })
    }

    // TODO: what else?
}
