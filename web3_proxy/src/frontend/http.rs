use crate::app::Web3ProxyApp;
use axum::{http::StatusCode, response::IntoResponse, Extension, Json};
use serde_json::json;
use std::sync::{atomic, Arc};

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
    // TODO: what else should we include? uptime?
    let body = json!({
        "total_queries": app.total_queries.load(atomic::Ordering::Relaxed),
        "active_queries_count": app.active_queries.entry_count(),
        "active_queries_size": app.active_queries.weighted_size(),
        "pending_transactions_count": app.pending_transactions.entry_count(),
        "pending_transactions_size": app.pending_transactions.weighted_size(),
        "user_cache_count": app.user_cache.entry_count(),
        "user_cache_size": app.user_cache.weighted_size(),
        "balanced_rpcs": app.balanced_rpcs,
        "private_rpcs": app.private_rpcs,
    });

    Json(body)
}
