//! Utlities for logging errors for admins and displaying errors to users.

use super::authorization::Authorization;
use crate::jsonrpc::JsonRpcForwardedResponse;
use axum::{
    headers,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use derive_more::From;
use http::header::InvalidHeaderValue;
use ipnet::AddrParseError;
use tracing::{trace, warn};
use migration::sea_orm::DbErr;
use redis_rate_limiter::redis::RedisError;
use reqwest::header::ToStrError;
use tokio::{sync::AcquireError, task::JoinError, time::Instant};

// TODO: take "IntoResponse" instead of Response?
pub type FrontendResult = Result<Response, FrontendErrorResponse>;

// TODO:
#[derive(Debug, From)]
pub enum FrontendErrorResponse {
    AccessDenied,
    Anyhow(anyhow::Error),
    SemaphoreAcquireError(AcquireError),
    Database(DbErr),
    HeadersError(headers::Error),
    HeaderToString(ToStrError),
    InvalidHeaderValue(InvalidHeaderValue),
    IpAddrParse(AddrParseError),
    JoinError(JoinError),
    NotFound,
    RateLimited(Authorization, Option<Instant>),
    Redis(RedisError),
    /// simple way to return an error message to the user and an anyhow to our logs
    StatusCode(StatusCode, String, Option<anyhow::Error>),
    /// TODO: what should be attached to the timout?
    Timeout(tokio::time::error::Elapsed),
    UlidDecodeError(ulid::DecodeError),
    UnknownKey,
}

impl FrontendErrorResponse {
    pub fn into_response_parts(self) -> (StatusCode, JsonRpcForwardedResponse) {
        match self {
            Self::AccessDenied => {
                // TODO: attach something to this trace. probably don't include much in the message though. don't want to leak creds by accident
                trace!("access denied");
                (
                    StatusCode::FORBIDDEN,
                    JsonRpcForwardedResponse::from_string(
                        // TODO: is it safe to expose all of our anyhow strings?
                        "FORBIDDEN".to_string(),
                        Some(StatusCode::FORBIDDEN.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::Anyhow(err) => {
                warn!("anyhow. err={:?}", err);
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
            // Self::(err) => {
            //     warn!("boxed err={:?}", err);
            //     (
            //         StatusCode::INTERNAL_SERVER_ERROR,
            //         JsonRpcForwardedResponse::from_str(
            //             // TODO: make this better. maybe include the error type?
            //             "boxed error!",
            //             Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
            //             None,
            //         ),
            //     )
            // }
            Self::Database(err) => {
                warn!("database err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcForwardedResponse::from_str(
                        "database error!",
                        Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::HeadersError(err) => {
                warn!("HeadersError {:?}", err);
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        &format!("{}", err),
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::IpAddrParse(err) => {
                warn!("IpAddrParse err={:?}", err);
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        &format!("{}", err),
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::InvalidHeaderValue(err) => {
                warn!("InvalidHeaderValue err={:?}", err);
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        &format!("{}", err),
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::JoinError(err) => {
                let code = if err.is_cancelled() {
                    trace!("JoinError. likely shutting down. err={:?}", err);
                    StatusCode::BAD_GATEWAY
                } else {
                    warn!("JoinError. err={:?}", err);
                    StatusCode::INTERNAL_SERVER_ERROR
                };

                (
                    code,
                    JsonRpcForwardedResponse::from_str(
                        // TODO: different messages, too?
                        "Unable to complete request",
                        Some(code.as_u16().into()),
                        None,
                    ),
                )
            }
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
            // TODO: this should actually by the id of the key. multiple users might control one key
            Self::RateLimited(authorization, retry_at) => {
                // TODO: emit a stat

                let retry_msg = if let Some(retry_at) = retry_at {
                    let retry_in = retry_at.duration_since(Instant::now()).as_secs();

                    format!(" Retry in {} seconds", retry_in)
                } else {
                    "".to_string()
                };

                // create a string with either the IP or the rpc_key_id
                let msg = if authorization.checks.rpc_secret_key_id.is_none() {
                    format!("too many requests from {}.{}", authorization.ip, retry_msg)
                } else {
                    format!(
                        "too many requests from rpc key #{}.{}",
                        authorization.checks.rpc_secret_key_id.unwrap(),
                        retry_msg
                    )
                };

                (
                    StatusCode::TOO_MANY_REQUESTS,
                    JsonRpcForwardedResponse::from_string(
                        msg,
                        Some(StatusCode::TOO_MANY_REQUESTS.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::Redis(err) => {
                warn!("redis err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcForwardedResponse::from_str(
                        "redis error!",
                        Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::SemaphoreAcquireError(err) => {
                warn!("semaphore acquire err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcForwardedResponse::from_string(
                        // TODO: is it safe to expose all of our anyhow strings?
                        "semaphore acquire error".to_string(),
                        Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::StatusCode(status_code, err_msg, err) => {
                // different status codes should get different error levels. 500s should warn. 400s should stat
                let code = status_code.as_u16();
                if (500..600).contains(&code) {
                    warn!("server error {} {:?}: {:?}", code, err_msg, err);
                } else {
                    trace!("user error {} {:?}: {:?}", code, err_msg, err);
                }

                (
                    status_code,
                    JsonRpcForwardedResponse::from_str(&err_msg, Some(code.into()), None),
                )
            }
            Self::Timeout(x) => (
                StatusCode::REQUEST_TIMEOUT,
                JsonRpcForwardedResponse::from_str(
                    &format!("request timed out: {:?}", x),
                    Some(StatusCode::REQUEST_TIMEOUT.as_u16().into()),
                    // TODO: include the actual id!
                    None,
                ),
            ),
            Self::HeaderToString(err) => {
                // trace!(?err, "HeaderToString");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        &format!("{}", err),
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::UlidDecodeError(err) => {
                // trace!(?err, "UlidDecodeError");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        &format!("{}", err),
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            // TODO: stat?
            Self::UnknownKey => (
                StatusCode::UNAUTHORIZED,
                JsonRpcForwardedResponse::from_str(
                    "unknown api key!",
                    Some(StatusCode::UNAUTHORIZED.as_u16().into()),
                    None,
                ),
            ),
        }
    }
}

impl IntoResponse for FrontendErrorResponse {
    fn into_response(self) -> Response {
        // TODO: include the request id in these so that users can give us something that will point to logs
        // TODO: status code is in the jsonrpc response and is also the first item in the tuple. DRY
        let (status_code, response) = self.into_response_parts();

        (status_code, Json(response)).into_response()
    }
}

pub async fn handler_404() -> Response {
    FrontendErrorResponse::NotFound.into_response()
}
