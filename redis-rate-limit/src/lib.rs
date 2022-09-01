//#![warn(missing_docs)]
mod errors;

use anyhow::Context;
use bb8_redis::redis::pipe;
use std::ops::Add;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::trace;

pub use crate::errors::{RedisError, RedisErrorSink};
pub use bb8_redis::{bb8, redis, RedisConnectionManager};

pub type RedisPool = bb8::Pool<RedisConnectionManager>;

pub struct RedisRateLimit {
    pool: RedisPool,
    key_prefix: String,
    /// The default maximum requests allowed in a period.
    max_requests_per_period: u64,
    /// seconds
    period: f32,
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
        max_requests_per_period: u64,
        period: f32,
    ) -> Self {
        let key_prefix = format!("{}:rrl:{}", app, label);

        Self {
            pool,
            key_prefix,
            max_requests_per_period,
            period,
        }
    }

    /// label might be an ip address or a user_key id.
    /// if setting max_per_period, be sure to keep the period the same for all requests to this label
    /// TODO:
    pub async fn throttle_label(
        &self,
        label: &str,
        max_per_period: Option<u64>,
        count: u64,
    ) -> anyhow::Result<ThrottleResult> {
        let max_per_period = max_per_period.unwrap_or(self.max_requests_per_period);

        if max_per_period == 0 {
            return Ok(ThrottleResult::RetryNever);
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("cannot tell the time")?
            .as_secs_f32();

        // if self.period is 60, period_id will be the minute of the current time
        let period_id = (now / self.period) % self.period;

        let throttle_key = format!("{}:{}:{}", self.key_prefix, label, period_id);

        let mut conn = self.pool.get().await?;

        let x: Vec<u64> = pipe()
            // we could get the key first, but that means an extra redis call for every check. this seems better
            .incr(&throttle_key, count)
            // set expiration the first time we set the key. ignore the result
            .expire(&throttle_key, self.period as usize)
            // .arg("NX")  // TODO: this works in redis, but not elasticache
            .ignore()
            // do the query
            .query_async(&mut *conn)
            .await
            .context("increment rate limit")?;

        let new_count = x.first().context("check rate limit result")?;

        if new_count > &max_per_period {
            let seconds_left_in_period = self.period - (now % self.period);

            let retry_at = Instant::now().add(Duration::from_secs_f32(seconds_left_in_period));

            trace!(%label, ?retry_at, "rate limited");

            return Ok(ThrottleResult::RetryAt(retry_at));
        }

        Ok(ThrottleResult::Allowed)
    }

    #[inline]
    pub async fn throttle(&self) -> anyhow::Result<ThrottleResult> {
        self.throttle_label("", None, 1).await
    }
}
