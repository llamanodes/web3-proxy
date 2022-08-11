use axum::response::Response;
use entities::user_keys;
use redis_cell_client::ThrottleResult;
use reqwest::StatusCode;
use sea_orm::{
    ColumnTrait, DeriveColumn, EntityTrait, EnumIter, IdenStatic, QueryFilter, QuerySelect,
};
use std::{net::IpAddr, time::Duration};
use tokio::time::Instant;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::app::{UserCacheValue, Web3ProxyApp};

use super::errors::anyhow_error_into_response;

pub enum RateLimitResult {
    AllowedIp(IpAddr),
    AllowedUser(i64),
    IpRateLimitExceeded(IpAddr),
    UserRateLimitExceeded(i64),
    UnknownKey,
}

impl RateLimitResult {
    // TODO: i think this should be a function on RateLimitResult
    pub async fn try_into_response(self) -> Result<RateLimitResult, Response> {
        match self {
            RateLimitResult::AllowedIp(_) => Ok(self),
            RateLimitResult::AllowedUser(_) => Ok(self),
            RateLimitResult::IpRateLimitExceeded(ip) => Err(anyhow_error_into_response(
                Some(StatusCode::TOO_MANY_REQUESTS),
                None,
                // TODO: how can we attach context here? maybe add a request id tracing field?
                anyhow::anyhow!(format!("rate limit exceeded for {}", ip)),
            )),
            RateLimitResult::UserRateLimitExceeded(user) => Err(anyhow_error_into_response(
                Some(StatusCode::TOO_MANY_REQUESTS),
                None,
                // TODO: don't expose numeric ids. show the address instead
                // TODO: how can we attach context here? maybe add a request id tracing field?
                anyhow::anyhow!(format!("rate limit exceeded for user {}", user)),
            )),
            RateLimitResult::UnknownKey => Err(anyhow_error_into_response(
                Some(StatusCode::FORBIDDEN),
                None,
                anyhow::anyhow!("unknown key"),
            )),
        }
    }
}

impl Web3ProxyApp {
    pub async fn rate_limit_by_ip(&self, ip: IpAddr) -> anyhow::Result<RateLimitResult> {
        let rate_limiter_key = format!("ip-{}", ip);

        // TODO: dry this up with rate_limit_by_key
        if let Some(rate_limiter) = &self.rate_limiter {
            match rate_limiter
                .throttle_key(&rate_limiter_key, None, None, None)
                .await
            {
                Ok(ThrottleResult::Allowed) => {}
                Ok(ThrottleResult::RetryAt(_retry_at)) => {
                    // TODO: set headers so they know when they can retry
                    debug!(?rate_limiter_key, "rate limit exceeded"); // this is too verbose, but a stat might be good
                                                                      // TODO: use their id if possible
                    return Ok(RateLimitResult::IpRateLimitExceeded(ip));
                }
                Err(err) => {
                    // internal error, not rate limit being hit
                    // TODO: i really want axum to do this for us in a single place.
                    return Err(err);
                }
            }
        } else {
            // TODO: if no redis, rate limit with a local cache?
            warn!("no rate limiter!");
        }

        Ok(RateLimitResult::AllowedIp(ip))
    }

    pub async fn rate_limit_by_key(&self, user_key: Uuid) -> anyhow::Result<RateLimitResult> {
        // check the local cache
        let user_data = if let Some(cached_user) = self.user_cache.read().get(&user_key) {
            // TODO: also include the time this value was last checked! otherwise we cache forever!
            if cached_user.expires_at < Instant::now() {
                // old record found
                None
            } else {
                // this key was active in the database recently
                Some(*cached_user)
            }
        } else {
            // cache miss
            None
        };

        // if cache was empty, check the database
        let user_data = if user_data.is_none() {
            if let Some(db) = &self.db_conn {
                /// helper enum for query just a few columns instead of the entire table
                #[derive(Copy, Clone, Debug, EnumIter, DeriveColumn)]
                enum QueryAs {
                    UserId,
                    RequestsPerMinute,
                }
                let user_data = match user_keys::Entity::find()
                    .select_only()
                    .column_as(user_keys::Column::UserId, QueryAs::UserId)
                    .column_as(
                        user_keys::Column::RequestsPerMinute,
                        QueryAs::RequestsPerMinute,
                    )
                    .filter(user_keys::Column::ApiKey.eq(user_key))
                    .filter(user_keys::Column::Active.eq(true))
                    .into_values::<_, QueryAs>()
                    .one(db)
                    .await?
                {
                    Some((user_id, requests_per_minute)) => {
                        UserCacheValue::from((
                            // TODO: how long should this cache last? get this from config
                            Instant::now() + Duration::from_secs(60),
                            user_id,
                            requests_per_minute,
                        ))
                    }
                    None => {
                        return Err(anyhow::anyhow!("unknown api key"));
                    }
                };

                //  save for the next run
                self.user_cache.write().insert(user_key, user_data);

                user_data
            } else {
                // TODO: rate limit with only local caches?
                unimplemented!("no cache hit and no database connection")
            }
        } else {
            // unwrap the cache's result
            user_data.unwrap()
        };

        // user key is valid. now check rate limits
        if let Some(rate_limiter) = &self.rate_limiter {
            // TODO: how does max burst actually work? what should it be?
            let user_max_burst = user_data.user_count_per_period / 3;
            let user_period = 60;

            if rate_limiter
                .throttle_key(
                    &user_key.to_string(),
                    Some(user_max_burst),
                    Some(user_data.user_count_per_period),
                    Some(user_period),
                )
                .await
                .is_err()
            {
                // TODO: set headers so they know when they can retry
                // warn!(?ip, "public rate limit exceeded");  // this is too verbose, but a stat might be good
                // TODO: use their id if possible
                // TODO: StatusCode::TOO_MANY_REQUESTS
                return Err(anyhow::anyhow!("too many requests from this key"));
            }
        } else {
            // TODO: if no redis, rate limit with a local cache?
            unimplemented!("no redis. cannot rate limit")
        }

        Ok(RateLimitResult::AllowedUser(user_data.user_id))
    }
}
