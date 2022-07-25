#![warn(missing_docs)]

use bb8_redis::redis::cmd;

pub use bb8_redis::redis::RedisError;
pub use bb8_redis::{bb8, RedisConnectionManager};

use std::time::Duration;

// TODO: take this as an argument to open?
const KEY_PREFIX: &str = "rate-limit";

pub type RedisClientPool = bb8::Pool<RedisConnectionManager>;

pub struct RedisCellClient {
    pool: RedisClientPool,
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
        pool: bb8::Pool<RedisConnectionManager>,
        default_key: String,
        max_burst: u32,
        count_per_period: u32,
        period: u32,
    ) -> Self {
        let default_key = format!("{}:{}", KEY_PREFIX, default_key);

        Self {
            pool,
            key: default_key,
            max_burst,
            count_per_period,
            period,
        }
    }

    #[inline]
    async fn _throttle(&self, key: &str, quantity: u32) -> Result<(), Duration> {
        let mut conn = self.pool.get().await.unwrap();

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
        let x: Vec<isize> = cmd("CL.THROTTLE")
            .arg(&(
                key,
                self.max_burst,
                self.count_per_period,
                self.period,
                quantity,
            ))
            .query_async(&mut *conn)
            .await
            .unwrap();

        assert_eq!(x.len(), 5);

        // TODO: trace log the result

        let retry_after = *x.get(3).unwrap();

        if retry_after == -1 {
            Ok(())
        } else {
            Err(Duration::from_secs(retry_after as u64))
        }
    }

    #[inline]
    pub async fn throttle(&self) -> Result<(), Duration> {
        self._throttle(&self.key, 1).await
    }

    #[inline]
    pub async fn throttle_key(&self, key: &str) -> Result<(), Duration> {
        let key = format!("{}:{}", self.key, key);

        self._throttle(key.as_ref(), 1).await
    }

    #[inline]
    pub async fn throttle_quantity(&self, quantity: u32) -> Result<(), Duration> {
        self._throttle(&self.key, quantity).await
    }
}
