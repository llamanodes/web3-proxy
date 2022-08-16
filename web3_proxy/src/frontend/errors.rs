use crate::jsonrpc::JsonRpcForwardedResponse;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use derive_more::From;
use redis_rate_limit::{bb8::RunError, RedisError};
use serde_json::value::RawValue;
use std::error::Error;

// TODO: take "IntoResult" instead?
pub type FrontendResult = Result<Response, FrontendErrorResponse>;

#[derive(From)]
pub enum FrontendErrorResponse {
    Anyhow(anyhow::Error),
    Box(Box<dyn Error>),
    // TODO: should we box these instead?
    Redis(RedisError),
    RedisRunError(RunError<RedisError>),
}

impl IntoResponse for FrontendErrorResponse {
    fn into_response(self) -> Response {
        let null_id = RawValue::from_string("null".to_string()).unwrap();

        // TODO: think more about this. this match should probably give us http and jsonrpc codes
        let err = match self {
            Self::Anyhow(err) => err,
            Self::Box(err) => anyhow::anyhow!("Boxed error: {:?}", err),
            Self::Redis(err) => err.into(),
            Self::RedisRunError(err) => err.into(),
        };

        let err = JsonRpcForwardedResponse::from_anyhow_error(err, null_id);

        let code = StatusCode::INTERNAL_SERVER_ERROR;

        // TODO: logs here are too verbose. emit a stat instead? or maybe only log internal errors?
        // warn!("Responding with error: {:?}", err);

        (code, Json(err)).into_response()
    }
}

pub async fn handler_404() -> Response {
    let err = anyhow::anyhow!("nothing to see here");

    anyhow_error_into_response(Some(StatusCode::NOT_FOUND), None, err)
}

/// TODO: generic error?
/// handle errors by converting them into something that implements `IntoResponse`
/// TODO: use this. i can't get <https://docs.rs/axum/latest/axum/error_handling/index.html> to work
/// TODO: i think we want a custom result type instead. put the anyhow result inside. then `impl IntoResponse for CustomResult`
pub fn anyhow_error_into_response(
    http_code: Option<StatusCode>,
    id: Option<Box<RawValue>>,
    err: anyhow::Error,
) -> Response {
    // TODO: we might have an id. like if this is for rate limiting, we can use it
    let id = id.unwrap_or_else(|| RawValue::from_string("null".to_string()).unwrap());

    let err = JsonRpcForwardedResponse::from_anyhow_error(err, id);

    // TODO: logs here are too verbose. emit a stat
    // warn!("Responding with error: {:?}", err);

    let code = http_code.unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    (code, Json(err)).into_response()
}
