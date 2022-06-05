use crate::jsonrpc::JsonRpcForwardedResponse;
use axum::{http::StatusCode, response::IntoResponse, Json};
use serde_json::value::RawValue;
use tracing::warn;

/// TODO: pretty 404 page? or us a json error fine?
pub async fn handler_404() -> impl IntoResponse {
    let err = anyhow::anyhow!("nothing to see here");

    handle_anyhow_error(err, Some(StatusCode::NOT_FOUND)).await
}

/// handle errors by converting them into something that implements `IntoResponse`
/// TODO: use this. i can't get https://docs.rs/axum/latest/axum/error_handling/index.html to work
pub async fn handle_anyhow_error(
    err: anyhow::Error,
    code: Option<StatusCode>,
) -> impl IntoResponse {
    // TODO: what id can we use? how do we make sure the incoming id gets attached to this?
    let id = RawValue::from_string("0".to_string()).unwrap();

    let err = JsonRpcForwardedResponse::from_anyhow_error(err, id);

    warn!("Responding with error: {:?}", err);

    let code = code.unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    (code, Json(err))
}

// i think we want a custom result type. it has an anyhow result inside. if it impl IntoResponse I think we'll get this for free
