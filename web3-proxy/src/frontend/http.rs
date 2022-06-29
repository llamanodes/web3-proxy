use axum::{http::StatusCode, response::IntoResponse, Extension, Json};
use serde_json::json;
use std::sync::Arc;

use crate::app::Web3ProxyApp;

/// Health check page for load balancers to use
pub async fn health(app: Extension<Arc<Web3ProxyApp>>) -> impl IntoResponse {
    if app.get_balanced_rpcs().has_synced_rpcs() {
        (StatusCode::OK, "OK")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, ":(")
    }
}

/// Very basic status page
pub async fn status(app: Extension<Arc<Web3ProxyApp>>) -> impl IntoResponse {
    // TODO: what else should we include? uptime? prometheus?
    let balanced_rpcs = app.get_balanced_rpcs();
    let private_rpcs = app.get_private_rpcs();
    let num_active_requests = app.get_active_requests().len();
    let num_pending_transactions = app.get_pending_transactions().len();

    let body = json!({
        "balanced_rpcs": balanced_rpcs,
        "private_rpcs": private_rpcs,
        "num_active_requests": num_active_requests,
        "num_pending_transactions": num_pending_transactions,
    });

    (StatusCode::INTERNAL_SERVER_ERROR, Json(body))
}
