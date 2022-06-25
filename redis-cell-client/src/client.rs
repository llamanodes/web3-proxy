use std::time::Duration;

use redis::aio::MultiplexedConnection;

// TODO: take this as an argument to open?
const KEY_PREFIX: &str = "rate-limit";

pub struct RedisCellClient {
    conn: MultiplexedConnection,
    key: String,
    max_burst: u32,
    count_per_period: u32,
    period: u32,
}

impl RedisCellClient {
    // todo: seems like this could be derived
    // TODO: take something generic for conn
    // TODO: use r2d2 for connection pooling?
    pub fn new(
        conn: MultiplexedConnection,
        key: String,
        max_burst: u32,
        count_per_period: u32,
        period: u32,
    ) -> Self {
        let key = format!("{}:{}", KEY_PREFIX, key);

        Self {
            conn,
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
        /*
        https://github.com/brandur/redis-cell#response

        CL.THROTTLE <key> <max_burst> <count per period> <period> [<quantity>]

        0. Whether the action was limited:
            0 indicates the action is allowed.
            1 indicates that the action was limited/blocked.
        1. The total limit of the key (max_burst + 1). This is equivalent to the common X-RateLimit-Limit HTTP header.
        2. The remaining limit of the key. Equivalent to X-RateLimit-Remaining.
        3. The number of seconds until the user should retry, and always -1 if the action was allowed. Equivalent to Retry-After.
        4. The number of seconds until the limit will reset to its maximum capacity. Equivalent to X-RateLimit-Reset.
        */
        // TODO: don't unwrap. maybe return anyhow::Result<Result<(), Duration>>
        // TODO: should we return more error info?
        let x: Vec<isize> = redis::cmd("CL.THROTTLE")
            .arg(&(
                &self.key,
                self.max_burst,
                self.count_per_period,
                self.period,
                quantity,
            ))
            .query_async(&mut self.conn.clone())
            .await
            .unwrap();

        assert_eq!(x.len(), 5);

        // TODO: trace log the result

        // TODO: maybe we should do #4
        let retry_after = *x.get(3).unwrap();

        if retry_after == -1 {
            Ok(())
        } else {
            Err(Duration::from_secs(retry_after as u64))
        }
    }
}
