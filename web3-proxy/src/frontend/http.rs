use axum::{http::StatusCode, response::IntoResponse, Extension, Json};
use serde_json::json;
use std::sync::Arc;

use crate::app::Web3ProxyApp;

/// Health check page for load balancers to use
pub async fn health(Extension(app): Extension<Arc<Web3ProxyApp>>) -> impl IntoResponse {
    if app.balanced_rpcs().synced() {
        (StatusCode::OK, "OK")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, ":(")
    }
}

/// Very basic status page
pub async fn status(Extension(app): Extension<Arc<Web3ProxyApp>>) -> impl IntoResponse {
    // TODO: what else should we include? uptime? prometheus?
    let balanced_rpcs = app.balanced_rpcs();
    let private_rpcs = app.private_rpcs();
    let num_active_requests = app.active_requests().len();
    let num_pending_transactions = app.pending_transactions().len();

    let body = json!({
        "balanced_rpcs": balanced_rpcs,
        "private_rpcs": private_rpcs,
        "num_active_requests": num_active_requests,
        "num_pending_transactions": num_pending_transactions,
    });

    (StatusCode::INTERNAL_SERVER_ERROR, Json(body))
}
