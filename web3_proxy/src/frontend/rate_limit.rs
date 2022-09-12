use super::errors::FrontendErrorResponse;
use crate::app::{UserCacheValue, Web3ProxyApp};
use anyhow::Context;
use entities::user_keys;
use redis_rate_limit::ThrottleResult;
use sea_orm::{
    ColumnTrait, DeriveColumn, EntityTrait, EnumIter, IdenStatic, QueryFilter, QuerySelect,
};
use std::{net::IpAddr, time::Duration};
use tokio::time::Instant;
use tracing::{debug, error, trace};
use uuid::Uuid;

#[derive(Debug)]
pub enum RateLimitResult {
    AllowedIp(IpAddr),
    AllowedUser(u64),
    RateLimitedIp(IpAddr, Option<Instant>),
    RateLimitedUser(u64, Option<Instant>),
    UnknownKey,
}

pub async fn rate_limit_by_ip(
    app: &Web3ProxyApp,
    ip: IpAddr,
) -> Result<IpAddr, FrontendErrorResponse> {
    match app.rate_limit_by_ip(ip).await? {
        RateLimitResult::AllowedIp(x) => Ok(x),
        RateLimitResult::RateLimitedIp(x, retry_at) => {
            Err(FrontendErrorResponse::RateLimitedIp(x, retry_at))
        }
        // TODO: don't panic. give the user an error
        x => unimplemented!("rate_limit_by_ip shouldn't ever see these: {:?}", x),
    }
}

pub async fn rate_limit_by_key(
    app: &Web3ProxyApp,
    // TODO: change this to a Ulid
    user_key: Uuid,
) -> Result<u64, FrontendErrorResponse> {
    match app.rate_limit_by_key(user_key).await? {
        RateLimitResult::AllowedUser(x) => Ok(x),
        RateLimitResult::RateLimitedUser(x, retry_at) => {
            Err(FrontendErrorResponse::RateLimitedUser(x, retry_at))
        }
        RateLimitResult::UnknownKey => Err(FrontendErrorResponse::UnknownKey),
        // TODO: don't panic. give the user an error
        x => unimplemented!("rate_limit_by_key shouldn't ever see these: {:?}", x),
    }
}

impl Web3ProxyApp {
    pub async fn rate_limit_by_ip(&self, ip: IpAddr) -> anyhow::Result<RateLimitResult> {
        // TODO: dry this up with rate_limit_by_key
        // TODO: have a local cache because if we hit redis too hard we get errors
        // TODO: query redis in the background so that users don't have to wait on this network request
        if let Some(rate_limiter) = &self.frontend_rate_limiter {
            let rate_limiter_label = format!("ip-{}", ip);

            match rate_limiter
                .throttle_label(&rate_limiter_label, None, 1)
                .await
            {
                Ok(ThrottleResult::Allowed) => Ok(RateLimitResult::AllowedIp(ip)),
                Ok(ThrottleResult::RetryAt(retry_at)) => {
                    // TODO: set headers so they know when they can retry
                    // TODO: debug or trace?
                    // this is too verbose, but a stat might be good
                    trace!(
                        ?rate_limiter_label,
                        "rate limit exceeded until {:?}",
                        retry_at
                    );
                    Ok(RateLimitResult::RateLimitedIp(ip, Some(retry_at)))
                }
                Ok(ThrottleResult::RetryNever) => {
                    // TODO: i don't think we'll get here. maybe if we ban an IP forever? seems unlikely
                    debug!(?rate_limiter_label, "rate limit exceeded");
                    Ok(RateLimitResult::RateLimitedIp(ip, None))
                }
                Err(err) => {
                    // internal error, not rate limit being hit
                    // TODO: i really want axum to do this for us in a single place.
                    error!(?err, "rate limiter is unhappy. allowing ip");
                    Ok(RateLimitResult::AllowedIp(ip))
                }
            }
        } else {
            // TODO: if no redis, rate limit with a local cache? "warn!" probably isn't right
            todo!("no rate limiter");
        }
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

        // save for the next run
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
        let user_data = match user_data {
            None => self
                .cache_user_data(user_key)
                .await
                .context("fetching user data for rate limits")?,
            Some(user_data) => user_data,
        };

        if user_data.user_id == 0 {
            return Ok(RateLimitResult::UnknownKey);
        }

        // user key is valid. now check rate limits
        // TODO: this is throwing errors when curve-api hits us with high concurrency. investigate i think its bb8's fault
        if false {
            if let Some(rate_limiter) = &self.frontend_rate_limiter {
                // TODO: query redis in the background so that users don't have to wait on this network request
                // TODO: better key? have a prefix so its easy to delete all of these
                // TODO: we should probably hash this or something
                let rate_limiter_label = format!("user-{}", user_key);

                match rate_limiter
                    .throttle_label(
                        &rate_limiter_label,
                        Some(user_data.user_count_per_period),
                        1,
                    )
                    .await
                {
                    Ok(ThrottleResult::Allowed) => {
                        Ok(RateLimitResult::AllowedUser(user_data.user_id))
                    }
                    Ok(ThrottleResult::RetryAt(retry_at)) => {
                        // TODO: set headers so they know when they can retry
                        // TODO: debug or trace?
                        // this is too verbose, but a stat might be good
                        trace!(
                            ?rate_limiter_label,
                            "rate limit exceeded until {:?}",
                            retry_at
                        );
                        Ok(RateLimitResult::RateLimitedUser(
                            user_data.user_id,
                            Some(retry_at),
                        ))
                    }
                    Ok(ThrottleResult::RetryNever) => {
                        // TODO: i don't think we'll get here. maybe if we ban an IP forever? seems unlikely
                        debug!(?rate_limiter_label, "rate limit exceeded");
                        Ok(RateLimitResult::RateLimitedUser(user_data.user_id, None))
                    }
                    Err(err) => {
                        // internal error, not rate limit being hit
                        // TODO: i really want axum to do this for us in a single place.
                        error!(?err, "rate limiter is unhappy. allowing ip");
                        Ok(RateLimitResult::AllowedUser(user_data.user_id))
                    }
                }
            } else {
                // TODO: if no redis, rate limit with a local cache?
                todo!("no redis. cannot rate limit")
            }
        } else {
            Ok(RateLimitResult::AllowedUser(user_data.user_id))
        }
    }
}
