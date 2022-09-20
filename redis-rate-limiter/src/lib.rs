//#![warn(missing_docs)]
use anyhow::Context;
use std::ops::Add;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{Duration, Instant};
use tracing::{debug, trace};

pub use deadpool_redis::redis;
pub use deadpool_redis::{
    Config as RedisConfig, Connection as RedisConnection, Manager as RedisManager,
    Pool as RedisPool, Runtime as DeadpoolRuntime,
};

#[derive(Clone)]
pub struct RedisRateLimiter {
    key_prefix: String,
    /// The default maximum requests allowed in a period.
    pub max_requests_per_period: u64,
    /// seconds
    pub period: f32,
    pool: RedisPool,
}

pub enum RedisRateLimitResult {
    Allowed(u64),
    RetryAt(Instant, u64),
    RetryNever,
}

impl RedisRateLimiter {
    pub fn new(
        app: &str,
        label: &str,
        max_requests_per_period: u64,
        period: f32,
        pool: RedisPool,
    ) -> Self {
        let key_prefix = format!("{}:rrl:{}", app, label);

        Self {
            pool,
            key_prefix,
            max_requests_per_period,
            period,
        }
    }

    pub fn now_as_secs(&self) -> f32 {
        // TODO: if system time doesn't match redis, this won't work great
        // TODO: now that we fixed
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("cannot tell the time")
            .as_secs_f32()
    }

    pub fn period_id(&self, now_as_secs: f32) -> f32 {
        (now_as_secs / self.period) % self.period
    }

    pub fn next_period(&self, now_as_secs: f32) -> Instant {
        let seconds_left_in_period = self.period - (now_as_secs % self.period);

        Instant::now().add(Duration::from_secs_f32(seconds_left_in_period))
    }

    /// label might be an ip address or a user_key id.
    /// if setting max_per_period, be sure to keep the period the same for all requests to this label
    pub async fn throttle_label(
        &self,
        label: &str,
        max_per_period: Option<u64>,
        count: u64,
    ) -> anyhow::Result<RedisRateLimitResult> {
        let max_per_period = max_per_period.unwrap_or(self.max_requests_per_period);

        if max_per_period == 0 {
            return Ok(RedisRateLimitResult::RetryNever);
        }

        let now = self.now_as_secs();

        // if self.period is 60, period_id will be the minute of the current time
        let period_id = self.period_id(now);

        // TODO: include max per period in the throttle key?
        let throttle_key = format!("{}:{}:{}", self.key_prefix, label, period_id);

        let mut conn = self
            .pool
            .get()
            .await
            .context("get redis connection for rate limits")?;

        // TODO: at high concurency, i think this is giving errors
        // TODO: i'm starting to think that bb8 has a bug
        let x: Vec<u64> = redis::pipe()
            // we could get the key first, but that means an extra redis call for every check. this seems better
            .incr(&throttle_key, count)
            // set expiration each time we set the key. ignore the result
            .expire(&throttle_key, self.period as usize)
            // TODO: NX will make it only set the expiration the first time. works in redis, but not elasticache
            // .arg("NX")
            .ignore()
            // do the query
            .query_async(&mut *conn)
            .await
            .context("increment rate limit")?;

        // TODO: is there a better way to do this?
        let new_count = *x.first().context("check rate limit result")?;

        if new_count > max_per_period {
            // TODO: this might actually be early if we are way over the count
            let retry_at = self.next_period(now);

            debug!(%label, ?retry_at, "rate limited: {}/{}", new_count, max_per_period);

            Ok(RedisRateLimitResult::RetryAt(retry_at, new_count))
        } else {
            trace!(%label, "NOT rate limited: {}/{}", new_count, max_per_period);
            Ok(RedisRateLimitResult::Allowed(new_count))
        }
    }

    #[inline]
    pub async fn throttle(&self) -> anyhow::Result<RedisRateLimitResult> {
        self.throttle_label("", None, 1).await
    }
}
