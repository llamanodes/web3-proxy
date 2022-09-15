//#![warn(missing_docs)]
use moka::future::Cache;
use redis_rate_limiter::{RedisRateLimitResult, RedisRateLimiter};
use std::cell::Cell;
use std::cmp::Eq;
use std::fmt::Display;
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{atomic::AtomicU64, Arc};
use tokio::time::Instant;

/// A local cache that sits in front of a RedisRateLimiter
/// Generic accross the key so it is simple to use with IPs or user keys
pub struct DeferredRateLimiter<K>
where
    K: Send + Sync,
{
    local_cache: Cache<K, Arc<AtomicU64>>,
    prefix: String,
    rrl: RedisRateLimiter,
}

pub enum DeferredRateLimitResult {
    Allowed,
    RetryAt(Instant),
    RetryNever,
}

impl<K> DeferredRateLimiter<K>
where
    K: Copy + Display + Hash + Eq + Send + Sync + 'static,
{
    pub fn new(cache_size: u64, prefix: &str, rrl: RedisRateLimiter) -> Self {
        Self {
            local_cache: Cache::new(cache_size),
            prefix: prefix.to_string(),
            rrl,
        }
    }

    /// if setting max_per_period, be sure to keep the period the same for all requests to this label
    pub async fn throttle(
        &self,
        key: &K,
        max_per_period: Option<u64>,
        count: u64,
    ) -> anyhow::Result<DeferredRateLimitResult> {
        let max_per_period = max_per_period.unwrap_or(self.rrl.max_requests_per_period);

        if max_per_period == 0 {
            return Ok(DeferredRateLimitResult::RetryNever);
        }

        let arc_new_entry = Arc::new(AtomicBool::new(false));
        let arc_retry_at = Arc::new(Cell::new(None));

        // TODO: DO NOT UNWRAP HERE. figure out how to handle anyhow error being wrapped in an Arc
        // TODO: i'm sure this could be a lot better. but race conditions make this hard to think through. brain needs sleep
        let key_count = {
            let arc_new_entry = arc_new_entry.clone();
            let arc_retry_at = arc_retry_at.clone();

            self.local_cache
                .try_get_with(*key, async move {
                    arc_new_entry.store(true, Ordering::Release);

                    let label = format!("{}:{}", self.prefix, key);

                    let redis_count = match self
                        .rrl
                        .throttle_label(&label, Some(max_per_period), count)
                        .await?
                    {
                        RedisRateLimitResult::Allowed(count) => count,
                        RedisRateLimitResult::RetryAt(retry_at, count) => {
                            arc_retry_at.set(Some(retry_at));
                            count
                        }
                        RedisRateLimitResult::RetryNever => unimplemented!(),
                    };

                    Ok::<_, anyhow::Error>(Arc::new(AtomicU64::new(redis_count)))
                })
                .await
                .unwrap()
        };

        if arc_new_entry.load(Ordering::Acquire) {
            // new entry
            if let Some(retry_at) = arc_retry_at.get() {
                Ok(DeferredRateLimitResult::RetryAt(retry_at))
            } else {
                Ok(DeferredRateLimitResult::Allowed)
            }
        } else {
            // we have a cached amount here

            // increment our local count if 

            let f = async move {
                let label = format!("{}:{}", self.prefix, key);

                let redis_count = match self
                    .rrl
                    .throttle_label(&label, Some(max_per_period), count)
                    .await?
                {
                    RedisRateLimitResult::Allowed(count) => todo!("do something with allow"),
                    RedisRateLimitResult::RetryAt(retry_at, count) => todo!("do something with retry at")
                    RedisRateLimitResult::RetryNever => unimplemented!(),
                };

                Ok::<_, anyhow::Error>(())
            };

            todo!("write more");
        }
    }
}
