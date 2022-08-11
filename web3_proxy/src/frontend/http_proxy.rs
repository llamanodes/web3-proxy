use axum::extract::Path;
use axum::response::Response;
use axum::{http::StatusCode, response::IntoResponse, Extension, Json};
use axum_client_ip::ClientIp;
use std::sync::Arc;
use uuid::Uuid;

use super::errors::handle_anyhow_error;
use super::rate_limit::handle_rate_limit_error_response;
use crate::{app::Web3ProxyApp, jsonrpc::JsonRpcRequestEnum};

pub async fn public_proxy_web3_rpc(
    Json(payload): Json<JsonRpcRequestEnum>,
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
) -> Response {
    if let Some(err_response) =
        handle_rate_limit_error_response(app.rate_limit_by_ip(&ip).await).await
    {
        return err_response.into_response();
    }

    match app.proxy_web3_rpc(payload).await {
        Ok(response) => (StatusCode::OK, Json(&response)).into_response(),
        Err(err) => handle_anyhow_error(None, None, err).into_response(),
    }
}

pub async fn user_proxy_web3_rpc(
    Json(payload): Json<JsonRpcRequestEnum>,
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    Path(user_key): Path<Uuid>,
) -> Response {
    // TODO: add a helper on this that turns RateLimitResult into error if its not allowed
    if let Some(err_response) =
        handle_rate_limit_error_response(app.rate_limit_by_key(user_key).await).await
    {
        return err_response.into_response();
    }

    match app.proxy_web3_rpc(payload).await {
        Ok(response) => (StatusCode::OK, Json(&response)).into_response(),
        Err(err) => handle_anyhow_error(None, None, err),
    }
}
