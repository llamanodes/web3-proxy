//#![warn(missing_docs)]
mod errors;

use anyhow::Context;
use bb8_redis::redis::pipe;
use std::ops::Add;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub use crate::errors::{RedisError, RedisErrorSink};
pub use bb8_redis::{bb8, redis, RedisConnectionManager};

pub type RedisPool = bb8::Pool<RedisConnectionManager>;

pub struct RedisRateLimit {
    pool: RedisPool,
    key_prefix: String,
    default_max_per_period: u64,
    period: u64,
}

pub enum ThrottleResult {
    Allowed,
    RetryAt(Instant),
    RetryNever,
}

impl RedisRateLimit {
    pub fn new(
        pool: RedisPool,
        app: &str,
        label: &str,
        default_max_per_period: u64,
        period: u64,
    ) -> Self {
        let key_prefix = format!("{}:rrl:{}", app, label);

        Self {
            pool,
            key_prefix,
            default_max_per_period,
            period,
        }
    }

    /// label might be an ip address or a user_key id
    pub async fn throttle_label(
        &self,
        label: &str,
        max_per_period: Option<u64>,
        count: u64,
    ) -> anyhow::Result<ThrottleResult> {
        let max_per_period = max_per_period.unwrap_or(self.default_max_per_period);

        if max_per_period == 0 {
            return Ok(ThrottleResult::RetryNever);
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("cannot tell the time")?
            .as_secs();

        // if self.period is 60, period_id will be the minute of the current time
        let period_id = (now / self.period) % self.period;

        let throttle_key = format!("{}:{}:{}", self.key_prefix, label, period_id);

        let mut conn = self.pool.get().await?;

        let x: Vec<u64> = pipe()
            // we could get the key first, but that means an extra redis call for every check. this seems better
            .incr(&throttle_key, count)
            // set expiration the first time we set the key. ignore the result
            .expire(&throttle_key, self.period.try_into().unwrap())
            // .arg("NX")  // TODO: this works in redis, but not elasticache
            .ignore()
            // do the query
            .query_async(&mut *conn)
            .await
            .context("increment rate limit")?;

        let new_count = x
            .first()
            .ok_or_else(|| anyhow::anyhow!("check rate limit result"))?;

        if new_count > &max_per_period {
            let seconds_left_in_period = self.period - now / self.period;

            let retry_at = Instant::now().add(Duration::from_secs(seconds_left_in_period));

            return Ok(ThrottleResult::RetryAt(retry_at));
        }

        Ok(ThrottleResult::Allowed)
    }

    #[inline]
    pub async fn throttle(&self) -> anyhow::Result<ThrottleResult> {
        self.throttle_label("", None, 1).await
    }
}
