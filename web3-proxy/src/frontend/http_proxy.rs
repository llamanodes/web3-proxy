use axum::{http::StatusCode, response::IntoResponse, Extension, Json};
use axum_client_ip::ClientIp;
use std::sync::Arc;

use super::errors::handle_anyhow_error;
use super::{rate_limit_by_ip, rate_limit_by_key};
use crate::{app::Web3ProxyApp, jsonrpc::JsonRpcRequestEnum};

pub async fn proxy_web3_rpc(
    Json(payload): Json<JsonRpcRequestEnum>,
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
) -> impl IntoResponse {
    if let Err(x) = rate_limit_by_ip(&app, &ip).await {
        return x.into_response();
    }

    match app.proxy_web3_rpc(payload).await {
        Ok(response) => (StatusCode::OK, Json(&response)).into_response(),
        Err(err) => handle_anyhow_error(None, None, err).await.into_response(),
    }
}

pub async fn user_proxy_web3_rpc(
    Json(payload): Json<JsonRpcRequestEnum>,
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    key: String,
) -> impl IntoResponse {
    if let Err(x) = rate_limit_by_key(&app, &key).await {
        return x.into_response();
    }

    match app.proxy_web3_rpc(payload).await {
        Ok(response) => (StatusCode::OK, Json(&response)).into_response(),
        Err(err) => handle_anyhow_error(None, None, err).await.into_response(),
    }
}
