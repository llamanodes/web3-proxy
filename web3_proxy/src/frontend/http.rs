use crate::app::Web3ProxyApp;
use axum::{http::StatusCode, response::IntoResponse, Extension, Json};
use serde_json::json;
use std::sync::Arc;

/// Health check page for load balancers to use
pub async fn health(Extension(app): Extension<Arc<Web3ProxyApp>>) -> impl IntoResponse {
    if app.balanced_rpcs.synced() {
        (StatusCode::OK, "OK")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, ":(")
    }
}

/// Very basic status page
/// TODO: replace this with proper stats and monitoring
pub async fn status(Extension(app): Extension<Arc<Web3ProxyApp>>) -> impl IntoResponse {
    // TODO: what else should we include? uptime? prometheus?
    let body = json!({
        "balanced_rpcs": app.balanced_rpcs,
        "private_rpcs": app.private_rpcs,
        "num_active_requests": app.active_requests.len(),
        "num_pending_transactions": app.pending_transactions.len(),
    });

    (StatusCode::OK, Json(body))
}
