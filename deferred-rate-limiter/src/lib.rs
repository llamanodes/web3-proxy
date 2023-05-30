//#![warn(missing_docs)]
use log::error;
use quick_cache_ttl::CacheWithTTL;
use redis_rate_limiter::{RedisRateLimitResult, RedisRateLimiter};
use std::cmp::Eq;
use std::fmt::{Debug, Display};
use std::hash::Hash;
use std::sync::atomic::Ordering;
use std::sync::{atomic::AtomicU64, Arc};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

/// A local cache that sits in front of a RedisRateLimiter
/// Generic accross the key so it is simple to use with IPs or user keys
pub struct DeferredRateLimiter<K>
where
    K: Send + Sync,
{
    local_cache: CacheWithTTL<K, Arc<AtomicU64>>,
    prefix: String,
    rrl: RedisRateLimiter,
    /// if None, defers to the max on rrl
    default_max_requests_per_period: Option<u64>,
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
    pub async fn new(
        // TODO: change this to cache_size in bytes
        cache_size: usize,
        prefix: &str,
        rrl: RedisRateLimiter,
        default_max_requests_per_second: Option<u64>,
    ) -> Self {
        let ttl = rrl.period as u64;

        // TODO: time to live is not exactly right. we want this ttl counter to start only after redis is down. this works for now
        // TODO: what do these weigh?
        // TODO: allow skipping max_capacity
        // TODO: prefix instead of a static str
        let local_cache = CacheWithTTL::new(
            "deferred rate limiter",
            cache_size,
            Duration::from_secs(ttl),
        )
        .await;

        Self {
            local_cache,
            prefix: prefix.to_string(),
            rrl,
            default_max_requests_per_period: default_max_requests_per_second,
        }
    }

    /// if setting max_per_period, be sure to keep the period the same for all requests to this label
    /// TODO: max_per_period being None means two things. some places it means unlimited, but here it means to use the default. make an enum
    pub async fn throttle(
        &self,
        key: K,
        max_requests_per_period: Option<u64>,
        count: u64,
    ) -> anyhow::Result<DeferredRateLimitResult> {
        let max_requests_per_period = max_requests_per_period.unwrap_or_else(|| {
            self.default_max_requests_per_period
                .unwrap_or(self.rrl.max_requests_per_period)
        });

        if max_requests_per_period == 0 {
            return Ok(DeferredRateLimitResult::RetryNever);
        }

        let deferred_rate_limit_result = Arc::new(Mutex::new(None));

        let redis_key = format!("{}:{}", self.prefix, key);

        // TODO: i'm sure this could be a lot better. but race conditions make this hard to think through. brain needs sleep
        let local_key_count: Arc<AtomicU64> = {
            // clone things outside of the `async move`
            let deferred_rate_limit_result = deferred_rate_limit_result.clone();
            let redis_key = redis_key.clone();
            let rrl = Arc::new(self.rrl.clone());

            // set arc_deferred_rate_limit_result and return the count
            self.local_cache
                .try_get_or_insert_async::<anyhow::Error, _>(&key, async move {
                    // we do not use the try operator here because we want to be okay with redis errors
                    let redis_count = match rrl
                        .throttle_label(&redis_key, Some(max_requests_per_period), count)
                        .await
                    {
                        Ok(RedisRateLimitResult::Allowed(count)) => {
                            let _ = deferred_rate_limit_result
                                .lock()
                                .await
                                .insert(DeferredRateLimitResult::Allowed);
                            count
                        }
                        Ok(RedisRateLimitResult::RetryAt(retry_at, count)) => {
                            let _ = deferred_rate_limit_result
                                .lock()
                                .await
                                .insert(DeferredRateLimitResult::RetryAt(retry_at));
                            count
                        }
                        Ok(RedisRateLimitResult::RetryNever) => {
                            unreachable!();
                        }
                        Err(err) => {
                            let _ = deferred_rate_limit_result
                                .lock()
                                .await
                                .insert(DeferredRateLimitResult::Allowed);

                            // if we get a redis error, just let the user through.
                            // if users are sticky on a server, local caches will work well enough
                            // though now that we do this, we need to reset rate limits every minute! cache must have ttl!
                            error!("unable to rate limit! creating empty cache. err={:?}", err);
                            0
                        }
                    };

                    Ok(Arc::new(AtomicU64::new(redis_count)))
                })
                .await?
        };

        let mut locked = deferred_rate_limit_result.lock().await;

        if let Some(deferred_rate_limit_result) = locked.take() {
            // new entry. redis was already incremented
            // return the retry_at that we got from
            Ok(deferred_rate_limit_result)
        } else {
            // we have a cached amount here
            let cached_key_count = local_key_count.fetch_add(count, Ordering::AcqRel);

            // assuming no other parallel futures incremented this key, this is the count that redis has
            let expected_key_count = cached_key_count + count;

            if expected_key_count > max_requests_per_period {
                // rate limit overshot!
                let now = self.rrl.now_as_secs();

                // do not fetch_sub
                // another row might have queued a redis throttle_label to keep our count accurate

                // show that we are rate limited without even querying redis
                let retry_at = self.rrl.next_period(now);
                Ok(DeferredRateLimitResult::RetryAt(retry_at))
            } else {
                // local caches think rate limit should be okay

                // prepare a future to update redis
                let rate_limit_f = {
                    let rrl = self.rrl.clone();
                    async move {
                        match rrl
                            .throttle_label(&redis_key, Some(max_requests_per_period), count)
                            .await
                        {
                            Ok(RedisRateLimitResult::Allowed(count)) => {
                                local_key_count.store(count, Ordering::Release);
                                DeferredRateLimitResult::Allowed
                            }
                            Ok(RedisRateLimitResult::RetryAt(retry_at, count)) => {
                                local_key_count.store(count, Ordering::Release);
                                DeferredRateLimitResult::RetryAt(retry_at)
                            }
                            Ok(RedisRateLimitResult::RetryNever) => {
                                // TODO: what should we do to arc_key_count?
                                DeferredRateLimitResult::RetryNever
                            }
                            Err(err) => {
                                // don't let redis errors block our users!
                                error!(
                                    "unable to query rate limits, but local cache is available. key={:?} err={:?}",
                                    key,
                                    err,
                                );
                                // TODO: we need to start a timer that resets this count every minute
                                DeferredRateLimitResult::Allowed
                            }
                        }
                    }
                };

                // if close to max_per_period, wait for redis
                // TODO: how close should we allow? depends on max expected concurent requests from one user
                let limit: f64 = (max_requests_per_period as f64 * 0.99)
                    .min(max_requests_per_period as f64 - 1.0);
                if expected_key_count > limit as u64 {
                    // close to period. don't risk it. wait on redis
                    Ok(rate_limit_f.await)
                } else {
                    // rate limit has enough headroom that it should be safe to do this in the background
                    // TODO: send an error here somewhere
                    tokio::spawn(rate_limit_f);

                    Ok(DeferredRateLimitResult::Allowed)
                }
            }
        }
    }
}
