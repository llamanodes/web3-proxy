use super::errors::FrontendErrorResponse;
use crate::app::{UserCacheValue, Web3ProxyApp};
use anyhow::Context;
use deferred_rate_limiter::DeferredRateLimitResult;
use entities::user_keys;
use sea_orm::{
    ColumnTrait, DeriveColumn, EntityTrait, EnumIter, IdenStatic, QueryFilter, QuerySelect,
};
use std::{net::IpAddr, sync::Arc};
use tokio::time::Instant;
use tracing::{error, trace};
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
        if let Some(rate_limiter) = &self.frontend_ip_rate_limiter {
            match rate_limiter.throttle(ip, None, 1).await {
                Ok(DeferredRateLimitResult::Allowed) => Ok(RateLimitResult::AllowedIp(ip)),
                Ok(DeferredRateLimitResult::RetryAt(retry_at)) => {
                    // TODO: set headers so they know when they can retry
                    // TODO: debug or trace?
                    // this is too verbose, but a stat might be good
                    trace!(?ip, "rate limit exceeded until {:?}", retry_at);
                    Ok(RateLimitResult::RateLimitedIp(ip, Some(retry_at)))
                }
                Ok(DeferredRateLimitResult::RetryNever) => {
                    // TODO: i don't think we'll get here. maybe if we ban an IP forever? seems unlikely
                    trace!(?ip, "rate limit is 0");
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

    pub(crate) async fn user_data(&self, user_key: Uuid) -> anyhow::Result<UserCacheValue> {
        let db = self.db_conn.as_ref().context("no database")?;

        let user_data: Result<_, Arc<anyhow::Error>> = self
            .user_cache
            .try_get_with(user_key, async move {
                /// helper enum for querying just a few columns instead of the entire table
                #[derive(Copy, Clone, Debug, EnumIter, DeriveColumn)]
                enum QueryAs {
                    UserId,
                    RequestsPerMinute,
                }

                // TODO: join the user table to this to return the User? we don't always need it
                match user_keys::Entity::find()
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
                        // TODO: add a column here for max, or is u64::MAX fine?
                        let user_count_per_period = if requests_per_minute == u64::MAX {
                            None
                        } else {
                            Some(requests_per_minute)
                        };

                        Ok(UserCacheValue::from((user_id, user_count_per_period)))
                    }
                    None => Ok(UserCacheValue::from((0, Some(0)))),
                }
            })
            .await;

        // TODO: i'm not actually sure about this expect
        user_data.map_err(|err| Arc::try_unwrap(err).expect("this should be the only reference"))
    }

    pub async fn rate_limit_by_key(&self, user_key: Uuid) -> anyhow::Result<RateLimitResult> {
        // check the local cache fo user data to save a database query
        let user_data = self.user_data(user_key).await?;

        if user_data.user_id == 0 {
            return Ok(RateLimitResult::UnknownKey);
        }

        let user_count_per_period = match user_data.user_count_per_period {
            None => return Ok(RateLimitResult::AllowedUser(user_data.user_id)),
            Some(x) => x,
        };

        // user key is valid. now check rate limits
        if let Some(rate_limiter) = &self.frontend_key_rate_limiter {
            match rate_limiter
                .throttle(user_key, Some(user_count_per_period), 1)
                .await
            {
                Ok(DeferredRateLimitResult::Allowed) => {
                    Ok(RateLimitResult::AllowedUser(user_data.user_id))
                }
                Ok(DeferredRateLimitResult::RetryAt(retry_at)) => {
                    // TODO: set headers so they know when they can retry
                    // TODO: debug or trace?
                    // this is too verbose, but a stat might be good
                    // TODO: keys are secrets! use the id instead
                    trace!(?user_key, "rate limit exceeded until {:?}", retry_at);
                    Ok(RateLimitResult::RateLimitedUser(
                        user_data.user_id,
                        Some(retry_at),
                    ))
                }
                Ok(DeferredRateLimitResult::RetryNever) => {
                    // TODO: keys are secret. don't log them!
                    trace!(?user_key, "rate limit is 0");
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
            // TODO: if no redis, rate limit with just a local cache?
            // if we don't have redis, we probably don't have a db, so this probably will never happen
            Err(anyhow::anyhow!("no redis. cannot rate limit"))
        }
    }
}
