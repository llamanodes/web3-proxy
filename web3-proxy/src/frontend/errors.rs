use axum::{http::StatusCode, response::IntoResponse, Json};
use serde_json::value::RawValue;
use tracing::warn;

use crate::jsonrpc::JsonRpcForwardedResponse;

/// TODO: pretty 404 page? or us a json error fine?
pub async fn handler_404() -> impl IntoResponse {
    let err = anyhow::anyhow!("nothing to see here");

    handle_anyhow_error(err, Some(StatusCode::NOT_FOUND)).await
}

/// handle errors by converting them into something that implements `IntoResponse`
/// TODO: use this. i can't get https://docs.rs/axum/latest/axum/error_handling/index.html to work
/// TODO: i think we want a custom result type instead. put the anyhow result inside. then `impl IntoResponse for CustomResult`
pub async fn handle_anyhow_error(
    err: anyhow::Error,
    code: Option<StatusCode>,
) -> impl IntoResponse {
    let id = RawValue::from_string("null".to_string()).unwrap();

    let err = JsonRpcForwardedResponse::from_anyhow_error(err, id);

    warn!("Responding with error: {:?}", err);

    let code = code.unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    (code, Json(err))
}
