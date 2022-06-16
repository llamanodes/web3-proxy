use axum::{http::StatusCode, response::IntoResponse, Extension, Json};
use std::sync::Arc;

use super::errors::handle_anyhow_error;
use crate::{app::Web3ProxyApp, jsonrpc::JsonRpcRequestEnum};

pub async fn proxy_web3_rpc(
    payload: Json<JsonRpcRequestEnum>,
    app: Extension<Arc<Web3ProxyApp>>,
) -> impl IntoResponse {
    match app.0.proxy_web3_rpc(payload.0).await {
        Ok(response) => (StatusCode::OK, Json(&response)).into_response(),
        Err(err) => handle_anyhow_error(err, None).await.into_response(),
    }
}
