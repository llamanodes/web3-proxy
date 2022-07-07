use axum::{http::StatusCode, response::IntoResponse, Extension, Json};
use axum_client_ip::ClientIp;
use std::sync::Arc;

use super::errors::handle_anyhow_error;
use crate::{app::Web3ProxyApp, jsonrpc::JsonRpcRequestEnum};

pub async fn proxy_web3_rpc(
    Json(payload): Json<JsonRpcRequestEnum>,
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
) -> impl IntoResponse {
    if let Some(rate_limiter) = app.get_public_rate_limiter() {
        let rate_limiter_key = format!("{}", ip);

        if rate_limiter.throttle_key(&rate_limiter_key).await.is_err() {
            // TODO: set headers so they know when they can retry
            // warn!(?ip, "public rate limit exceeded");
            // TODO: use their id if possible
            return handle_anyhow_error(
                Some(StatusCode::TOO_MANY_REQUESTS),
                None,
                anyhow::anyhow!("too many requests"),
            )
            .await
            .into_response();
        }
    } else {
        // TODO: if no redis, rate limit with a local cache?
    }

    match app.proxy_web3_rpc(payload).await {
        Ok(response) => (StatusCode::OK, Json(&response)).into_response(),
        Err(err) => handle_anyhow_error(None, None, err).await.into_response(),
    }
}
