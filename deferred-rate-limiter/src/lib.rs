//#![warn(missing_docs)]
use moka::future::Cache;
use redis_rate_limiter::{RedisRateLimitResult, RedisRateLimiter};
use std::cell::Cell;
use std::cmp::Eq;
use std::fmt::{Debug, Display};
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{atomic::AtomicU64, Arc};
use tokio::time::Instant;
use tracing::error;

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
    K: Copy + Debug + Display + Hash + Eq + Send + Sync + 'static,
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

        let redis_key = format!("{}:{}", self.prefix, key);

        // TODO: DO NOT UNWRAP HERE. figure out how to handle anyhow error being wrapped in an Arc
        // TODO: i'm sure this could be a lot better. but race conditions make this hard to think through. brain needs sleep
        let arc_key_count: Arc<AtomicU64> = {
            // clone things outside of the
            let arc_new_entry = arc_new_entry.clone();
            let arc_retry_at = arc_retry_at.clone();
            let redis_key = redis_key.clone();
            let rrl = Arc::new(self.rrl.clone());

            self.local_cache.get_with(*key, async move { todo!() });

            /*
            let x = self
                .local_cache
                .get_with(*key, move {
                    async move {
                        arc_new_entry.store(true, Ordering::Release);

                        // we do not use the try operator here because we want to be okay with redis errors
                        let redis_count = match rrl
                            .throttle_label(&redis_key, Some(max_per_period), count)
                            .await
                        {
                            Ok(RedisRateLimitResult::Allowed(count)) => count,
                            Ok(RedisRateLimitResult::RetryAt(retry_at, count)) => {
                                arc_retry_at.set(Some(retry_at));
                                count
                            }
                            Ok(RedisRateLimitResult::RetryNever) => unimplemented!(),
                            Err(x) => 0, // Err(err) => todo!("allow rate limit errors"),
                                         // Err(RedisSomething)
                                         // if we get a redis error, just let the user through. local caches will work fine
                                         // though now that we do this, we need to reset rate limits every minute!
                        };

                        Arc::new(AtomicU64::new(redis_count))
                    }
                })
                .await;
            */

            todo!("write more")
        };

        if arc_new_entry.load(Ordering::Acquire) {
            // new entry. redis was already incremented
            // return the retry_at that we got from
            if let Some(retry_at) = arc_retry_at.get() {
                Ok(DeferredRateLimitResult::RetryAt(retry_at))
            } else {
                Ok(DeferredRateLimitResult::Allowed)
            }
        } else {
            // we have a cached amount here
            let cached_key_count = arc_key_count.fetch_add(count, Ordering::Acquire);

            // assuming no other parallel futures incremented this key, this is the count that redis has
            let expected_key_count = cached_key_count + count;

            if expected_key_count > max_per_period {
                // rate limit overshot!
                let now = self.rrl.now_as_secs();

                // do not fetch_sub
                // another row might have queued a redis throttle_label to keep our count accurate

                // show that we are rate limited without even querying redis
                let retry_at = self.rrl.next_period(now);
                return Ok(DeferredRateLimitResult::RetryAt(retry_at));
            } else {
                // local caches think rate limit should be okay

                // prepare a future to increment redis
                let increment_redis_f = {
                    let rrl = self.rrl.clone();
                    async move {
                        let rate_limit_result = rrl
                            .throttle_label(&redis_key, Some(max_per_period), count)
                            .await?;

                        // TODO: log bad responses?

                        Ok::<_, anyhow::Error>(rate_limit_result)
                    }
                };

                // if close to max_per_period, wait for redis
                // TODO: how close should we allow? depends on max expected concurent requests from one user
                if expected_key_count > max_per_period * 98 / 100 {
                    // close to period. don't risk it. wait on redis
                    // match increment_redis_f.await {
                    //     Ok(RedisRateLimitResult::Allowed(redis_count)) => todo!("1"),
                    //     Ok(RedisRateLimitResult::RetryAt(retry_at, redis_count)) => todo!("2"),
                    //     Ok(RedisRateLimitResult::RetryNever) => todo!("3"),
                    //     Err(err) => {
                    //         // don't let redis errors block our users!
                    //         // error!(
                    //         //     // ?key,  // TODO: this errors
                    //         //     ?err,
                    //         //     "unable to query rate limits. local cache available"
                    //         // );
                    //         todo!("4");
                    //     }
                    // };
                    todo!("i think we can't move f around like this");
                } else {
                    // rate limit has enough headroom that it should be safe to do this in the background
                    tokio::spawn(increment_redis_f);
                }
            }

            let new_count = arc_key_count.fetch_add(count, Ordering::Release);

            todo!("check new_count");

            // increment our local count and check what happened
            // THERE IS A SMALL RACE HERE, but thats the price we pay for speed. a few queries leaking through is okay
            // if it becomes a problem, we can lock, but i don't love that. it might be short enough to be okay though

            todo!("write more");
        }
    }
}
