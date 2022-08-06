//#![warn(missing_docs)]

use bb8_redis::redis::cmd;

pub use bb8_redis::redis::RedisError;
pub use bb8_redis::{bb8, RedisConnectionManager};

use std::time::Duration;

pub type RedisClientPool = bb8::Pool<RedisConnectionManager>;

pub struct RedisCellClient {
    pool: RedisClientPool,
    key: String,
    default_max_burst: u32,
    default_count_per_period: u32,
    default_period: u32,
}

impl RedisCellClient {
    // todo: seems like this could be derived
    // TODO: take something generic for conn
    // TODO: use r2d2 for connection pooling?
    pub fn new(
        pool: bb8::Pool<RedisConnectionManager>,
        app: &str,
        key: &str,
        default_max_burst: u32,
        default_count_per_period: u32,
        default_period: u32,
    ) -> Self {
        let key = format!("{}:redis-cell:{}", app, key);

        Self {
            pool,
            key,
            default_max_burst,
            default_count_per_period,
            default_period,
        }
    }

    #[inline]
    async fn _throttle(
        &self,
        key: &str,
        max_burst: Option<u32>,
        count_per_period: Option<u32>,
        period: Option<u32>,
        quantity: u32,
    ) -> Result<(), Option<Duration>> {
        let mut conn = self.pool.get().await.unwrap();

        let count_per_period = count_per_period.unwrap_or(self.default_count_per_period);

        if count_per_period == 0 {
            return Err(None);
        }

        let max_burst = max_burst.unwrap_or(self.default_max_burst);
        let period = period.unwrap_or(self.default_period);

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
            .arg(&(key, max_burst, count_per_period, period, quantity))
            .query_async(&mut *conn)
            .await
            .unwrap();

        assert_eq!(x.len(), 5);

        // TODO: trace log the result

        let retry_after = *x.get(3).unwrap();

        if retry_after == -1 {
            Ok(())
        } else {
            Err(Some(Duration::from_secs(retry_after as u64)))
        }
    }

    #[inline]
    pub async fn throttle(&self) -> Result<(), Option<Duration>> {
        self._throttle(&self.key, None, None, None, 1).await
    }

    #[inline]
    pub async fn throttle_key(
        &self,
        key: &str,
        max_burst: Option<u32>,
        count_per_period: Option<u32>,
        period: Option<u32>,
    ) -> Result<(), Option<Duration>> {
        let key = format!("{}:{}", self.key, key);

        self._throttle(key.as_ref(), max_burst, count_per_period, period, 1)
            .await
    }

    #[inline]
    pub async fn throttle_quantity(&self, quantity: u32) -> Result<(), Option<Duration>> {
        self._throttle(&self.key, None, None, None, quantity).await
    }
}
