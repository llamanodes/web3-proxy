use axum::response::IntoResponse;
use entities::user_keys;
use redis_cell_client::ThrottleResult;
use reqwest::StatusCode;
use sea_orm::{
    ColumnTrait, DeriveColumn, EntityTrait, EnumIter, IdenStatic, QueryFilter, QuerySelect,
};
use std::net::IpAddr;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::app::Web3ProxyApp;

use super::errors::handle_anyhow_error;

pub async fn rate_limit_by_ip(app: &Web3ProxyApp, ip: &IpAddr) -> Result<(), impl IntoResponse> {
    let rate_limiter_key = format!("ip-{}", ip);

    // TODO: dry this up with rate_limit_by_key
    if let Some(rate_limiter) = app.rate_limiter() {
        match rate_limiter
            .throttle_key(&rate_limiter_key, None, None, None)
            .await
        {
            Ok(ThrottleResult::Allowed) => {}
            Ok(ThrottleResult::RetryAt(_retry_at)) => {
                // TODO: set headers so they know when they can retry
                debug!(?rate_limiter_key, "rate limit exceeded"); // this is too verbose, but a stat might be good
                                                                  // TODO: use their id if possible
                return Err(handle_anyhow_error(
                    Some(StatusCode::TOO_MANY_REQUESTS),
                    None,
                    anyhow::anyhow!(format!("too many requests from this ip: {}", ip)),
                )
                .await
                .into_response());
            }
            Err(err) => {
                // internal error, not rate limit being hit
                // TODO: i really want axum to do this for us in a single place.
                return Err(handle_anyhow_error(
                    Some(StatusCode::INTERNAL_SERVER_ERROR),
                    None,
                    err,
                )
                .await
                .into_response());
            }
        }
    } else {
        // TODO: if no redis, rate limit with a local cache?
        warn!("no rate limiter!");
    }

    Ok(())
}

/// if Ok(()), rate limits are acceptable
/// if Err(response), rate limits exceeded
pub async fn rate_limit_by_key(
    app: &Web3ProxyApp,
    user_key: Uuid,
) -> Result<(), impl IntoResponse> {
    let db = app.db_conn();

    /// query just a few columns instead of the entire table
    #[derive(Copy, Clone, Debug, EnumIter, DeriveColumn)]
    enum QueryAs {
        UserId,
        RequestsPerMinute,
    }

    // query the db to make sure this key is active
    // TODO: probably want a cache on this
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
        .await
    {
        Ok::<Option<(i64, u32)>, _>(Some((_user_id, user_count_per_period))) => {
            // user key is valid
            if let Some(rate_limiter) = app.rate_limiter() {
                // TODO: how does max burst actually work? what should it be?
                let user_max_burst = user_count_per_period / 3;
                let user_period = 60;

                if rate_limiter
                    .throttle_key(
                        &user_key.to_string(),
                        Some(user_max_burst),
                        Some(user_count_per_period),
                        Some(user_period),
                    )
                    .await
                    .is_err()
                {
                    // TODO: set headers so they know when they can retry
                    // warn!(?ip, "public rate limit exceeded");  // this is too verbose, but a stat might be good
                    // TODO: use their id if possible
                    return Err(handle_anyhow_error(
                        Some(StatusCode::TOO_MANY_REQUESTS),
                        None,
                        // TODO: include the user id (NOT THE API KEY!) here
                        anyhow::anyhow!("too many requests from this key"),
                    )
                    .await
                    .into_response());
                }
            } else {
                // TODO: if no redis, rate limit with a local cache?
            }
        }
        Ok(None) => {
            // invalid user key
            // TODO: rate limit by ip here, too? maybe tarpit?
            return Err(handle_anyhow_error(
                Some(StatusCode::FORBIDDEN),
                None,
                anyhow::anyhow!("unknown api key"),
            )
            .await
            .into_response());
        }
        Err(err) => {
            let err: anyhow::Error = err.into();

            return Err(handle_anyhow_error(
                Some(StatusCode::INTERNAL_SERVER_ERROR),
                None,
                err.context("failed checking database for user key"),
            )
            .await
            .into_response());
        }
    }

    Ok(())
}
