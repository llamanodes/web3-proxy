use super::errors::{anyhow_error_into_response, FrontendErrorResponse};
use crate::app::{UserCacheValue, Web3ProxyApp};
use anyhow::Context;
use axum::response::Response;
use derive_more::From;
use entities::user_keys;
use redis_rate_limit::ThrottleResult;
use reqwest::StatusCode;
use sea_orm::{
    ColumnTrait, DeriveColumn, EntityTrait, EnumIter, IdenStatic, QueryFilter, QuerySelect,
};
use std::{net::IpAddr, time::Duration};
use tokio::time::Instant;
use tracing::debug;
use uuid::Uuid;

pub enum RateLimitResult {
    AllowedIp(IpAddr),
    AllowedUser(u64),
    IpRateLimitExceeded(IpAddr),
    UserRateLimitExceeded(u64),
    UnknownKey,
}

#[derive(From)]
pub enum RequestFrom {
    Ip(IpAddr),
    // TODO: fetch the actual user?
    User(u64),
}

impl TryFrom<RequestFrom> for IpAddr {
    type Error = anyhow::Error;

    fn try_from(value: RequestFrom) -> Result<Self, Self::Error> {
        match value {
            RequestFrom::Ip(x) => Ok(x),
            _ => Err(anyhow::anyhow!("not an ip")),
        }
    }
}

impl TryFrom<RequestFrom> for u64 {
    type Error = anyhow::Error;

    fn try_from(value: RequestFrom) -> Result<Self, Self::Error> {
        match value {
            RequestFrom::User(x) => Ok(x),
            _ => Err(anyhow::anyhow!("not a user")),
        }
    }
}

pub async fn rate_limit_by_ip(
    app: &Web3ProxyApp,
    ip: IpAddr,
) -> Result<IpAddr, FrontendErrorResponse> {
    let rate_limit_result = app.rate_limit_by_ip(ip).await?;

    match rate_limit_result {
        RateLimitResult::AllowedIp(x) => Ok(x),
        RateLimitResult::AllowedUser(_) => panic!("only ips or errors are expected here"),
        rate_limit_result => {
            let _: RequestFrom = rate_limit_result.try_into()?;

            panic!("try_into should have failed")
        }
    }
}

pub async fn rate_limit_by_user_key(
    app: &Web3ProxyApp,
    // TODO: change this to a Ulid
    user_key: Uuid,
) -> Result<u64, FrontendErrorResponse> {
    let rate_limit_result = app.rate_limit_by_key(user_key).await?;

    match rate_limit_result {
        RateLimitResult::AllowedIp(_) => panic!("only user keys or errors are expected here"),
        RateLimitResult::AllowedUser(x) => Ok(x),
        rate_limit_result => {
            let _: RequestFrom = rate_limit_result.try_into()?;

            panic!("try_into should have failed")
        }
    }
}

impl TryFrom<RateLimitResult> for RequestFrom {
    // TODO: return an error that has its own IntoResponse?
    type Error = Response;

    fn try_from(value: RateLimitResult) -> Result<Self, Self::Error> {
        match value {
            RateLimitResult::AllowedIp(x) => Ok(RequestFrom::Ip(x)),
            RateLimitResult::AllowedUser(x) => Ok(RequestFrom::User(x)),
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
        // TODO: dry this up with rate_limit_by_key
        if let Some(rate_limiter) = &self.rate_limiter {
            let rate_limiter_label = format!("ip-{}", ip);

            // TODO: query redis in the background so that users don't have to wait on this network request
            match rate_limiter
                .throttle_label(&rate_limiter_label, None, 1)
                .await
            {
                Ok(ThrottleResult::Allowed) => {}
                Ok(ThrottleResult::RetryAt(_retry_at)) => {
                    // TODO: set headers so they know when they can retry
                    debug!(?rate_limiter_label, "rate limit exceeded"); // this is too verbose, but a stat might be good
                                                                        // TODO: use their id if possible
                    return Ok(RateLimitResult::IpRateLimitExceeded(ip));
                }
                Ok(ThrottleResult::RetryNever) => {
                    // TODO: prettier error for the user
                    return Err(anyhow::anyhow!("blocked by rate limiter"));
                }
                Err(err) => {
                    // internal error, not rate limit being hit
                    // TODO: i really want axum to do this for us in a single place.
                    return Err(err);
                }
            }
        } else {
            // TODO: if no redis, rate limit with a local cache? "warn!" probably isn't right
            todo!("no rate limiter");
        }

        Ok(RateLimitResult::AllowedIp(ip))
    }

    pub(crate) async fn cache_user_data(&self, user_key: Uuid) -> anyhow::Result<UserCacheValue> {
        let db = self.db_conn.as_ref().context("no database")?;

        /// helper enum for query just a few columns instead of the entire table
        #[derive(Copy, Clone, Debug, EnumIter, DeriveColumn)]
        enum QueryAs {
            UserId,
            RequestsPerMinute,
        }
        // TODO: join the user table to this to return the User? we don't always need it
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
                // TODO: think about this more
                UserCacheValue::from((
                    // TODO: how long should this cache last? get this from config
                    Instant::now() + Duration::from_secs(60),
                    0,
                    0,
                ))
            }
        };

        //  save for the next run
        self.user_cache.insert(user_key, user_data).await;

        Ok(user_data)
    }

    pub async fn rate_limit_by_key(&self, user_key: Uuid) -> anyhow::Result<RateLimitResult> {
        // check the local cache
        let user_data = if let Some(cached_user) = self.user_cache.get(&user_key) {
            // TODO: also include the time this value was last checked! otherwise we cache forever!
            if cached_user.expires_at < Instant::now() {
                // old record found
                None
            } else {
                // this key was active in the database recently
                Some(cached_user)
            }
        } else {
            // cache miss
            None
        };

        // if cache was empty, check the database
        // TODO: i think there is a cleaner way to do this
        let user_data = if user_data.is_none() {
            self.cache_user_data(user_key)
                .await
                .context("no user data")?
        } else {
            // unwrap the cache's result
            user_data.context("no user data")?
        };

        if user_data.user_id == 0 {
            return Err(anyhow::anyhow!("unknown key!"));
        }

        // user key is valid. now check rate limits
        if let Some(rate_limiter) = &self.rate_limiter {
            // TODO: query redis in the background so that users don't have to wait on this network request
            if rate_limiter
                .throttle_label(
                    &user_key.to_string(),
                    Some(user_data.user_count_per_period),
                    1,
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
            todo!("no redis. cannot rate limit")
        }

        Ok(RateLimitResult::AllowedUser(user_data.user_id))
    }
}
