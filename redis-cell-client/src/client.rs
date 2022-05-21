use std::time::Duration;

use redis::aio::MultiplexedConnection;

// TODO: take this as an argument to open?
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

    // CL.THROTTLE <key> <max_burst> <count per period> <period> [<quantity>]
    /*


    0. Whether the action was limited:
        0 indicates the action is allowed.
        1 indicates that the action was limited/blocked.
    1. The total limit of the key (max_burst + 1). This is equivalent to the common X-RateLimit-Limit HTTP header.
    2. The remaining limit of the key. Equivalent to X-RateLimit-Remaining.
    3. The number of seconds until the user should retry, and always -1 if the action was allowed. Equivalent to Retry-After.
    4. The number of seconds until the limit will reset to its maximum capacity. Equivalent to X-RateLimit-Reset.

    */
    pub async fn throttle(
        &self,
        key: &str,
        max_burst: u32,
        count_per_period: u32,
        period: u32,
        quantity: u32,
    ) -> Result<(), Duration> {
        // TODO: should we return more error info?
        // https://github.com/brandur/redis-cell#response

        let mut conn = self.conn.clone();

        // TODO: don't unwrap. maybe return Option<Duration>
        let x: Vec<isize> = redis::cmd("CL.THROTTLE")
            .arg(&(key, max_burst, count_per_period, period, quantity))
            .query_async(&mut conn)
            .await
            .unwrap();

        assert_eq!(x.len(), 5);

        let retry_after = *x.get(3).unwrap();

        if retry_after == -1 {
            Ok(())
        } else {
            Err(Duration::from_secs(retry_after as u64))
        }
    }

    // TODO: what else?
}

pub struct RedisCellKey {
    client: RedisCellClient,
    key: String,
    max_burst: u32,
    count_per_period: u32,
    period: u32,
}

impl RedisCellKey {
    // todo: seems like this could be derived
    pub fn new(
        client: &RedisCellClient,
        key: String,
        max_burst: u32,
        count_per_period: u32,
        period: u32,
    ) -> Self {
        let key = format!("{}:{}", KEY_PREFIX, key);

        Self {
            client: client.clone(),
            key,
            max_burst,
            count_per_period,
            period,
        }
    }

    #[inline]
    pub async fn throttle(&self) -> Result<(), Duration> {
        self.throttle_quantity(1).await
    }

    #[inline]
    pub async fn throttle_quantity(&self, quantity: u32) -> Result<(), Duration> {
        self.client
            .throttle(
                &self.key,
                self.max_burst,
                self.count_per_period,
                self.period,
                quantity,
            )
            .await
    }
}
