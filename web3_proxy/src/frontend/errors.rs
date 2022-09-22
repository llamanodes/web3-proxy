use crate::{app::UserKeyData, jsonrpc::JsonRpcForwardedResponse};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use derive_more::From;
use redis_rate_limiter::redis::RedisError;
use sea_orm::DbErr;
use std::{error::Error, net::IpAddr};
use tokio::time::Instant;
use tracing::warn;

// TODO: take "IntoResult" instead?
pub type FrontendResult = Result<Response, FrontendErrorResponse>;

#[derive(From)]
pub enum FrontendErrorResponse {
    Anyhow(anyhow::Error),
    Box(Box<dyn Error>),
    Redis(RedisError),
    Response(Response),
    Database(DbErr),
    RateLimitedUser(UserKeyData, Option<Instant>),
    RateLimitedIp(IpAddr, Option<Instant>),
    UnknownKey,
    NotFound,
}

impl IntoResponse for FrontendErrorResponse {
    fn into_response(self) -> Response {
        // TODO: include the request id in these so that users can give us something that will point to logs
        let (status_code, response) = match self {
            Self::Anyhow(err) => {
                warn!(?err, "anyhow");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcForwardedResponse::from_string(
                        // TODO: is it safe to expose all of our anyhow strings?
                        err.to_string(),
                        Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::Box(err) => {
                warn!(?err, "boxed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcForwardedResponse::from_str(
                        // TODO: make this better. maybe include the error type?
                        "boxed error!",
                        Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::Redis(err) => {
                warn!(?err, "redis");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcForwardedResponse::from_str(
                        "redis error!",
                        Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::Response(r) => {
                debug_assert_ne!(r.status(), StatusCode::OK);
                return r;
            }
            Self::Database(err) => {
                warn!(?err, "database");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcForwardedResponse::from_str(
                        "database error!",
                        Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::RateLimitedIp(ip, retry_at) => {
                // TODO: emit a stat
                // TODO: include retry_at in the error
                // TODO: if retry_at is None, give an unauthorized status code?
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    JsonRpcForwardedResponse::from_string(
                        format!("too many requests from ip {}!", ip),
                        Some(StatusCode::TOO_MANY_REQUESTS.as_u16().into()),
                        None,
                    ),
                )
            }
            // TODO: this should actually by the id of the key. multiple users might control one key
            Self::RateLimitedUser(user_data, retry_at) => {
                // TODO: emit a stat
                // TODO: include retry_at in the error
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    JsonRpcForwardedResponse::from_string(
                        // TODO: better error
                        format!("too many requests from {:?}!", user_data),
                        Some(StatusCode::TOO_MANY_REQUESTS.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::UnknownKey => (
                StatusCode::UNAUTHORIZED,
                JsonRpcForwardedResponse::from_str(
                    "unknown api key!",
                    Some(StatusCode::UNAUTHORIZED.as_u16().into()),
                    None,
                ),
            ),
            Self::NotFound => {
                // TODO: emit a stat?
                // TODO: instead of an error, show a normal html page for 404
                (
                    StatusCode::NOT_FOUND,
                    JsonRpcForwardedResponse::from_str(
                        "not found!",
                        Some(StatusCode::NOT_FOUND.as_u16().into()),
                        None,
                    ),
                )
            }
        };

        (status_code, Json(response)).into_response()
    }
}

pub async fn handler_404() -> Response {
    FrontendErrorResponse::NotFound.into_response()
}
