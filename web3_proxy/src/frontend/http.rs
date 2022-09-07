use crate::app::Web3ProxyApp;
use axum::{http::StatusCode, response::IntoResponse, Extension, Json};
use moka::future::ConcurrentCacheExt;
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
    app.pending_transactions.sync();
    app.user_cache.sync();

    let body = json!({
        "total_queries": app.total_queries.load(atomic::Ordering::Acquire),
        "pending_transactions_count": app.pending_transactions.entry_count(),
        "pending_transactions_size": app.pending_transactions.weighted_size(),
        "user_cache_count": app.user_cache.entry_count(),
        "user_cache_size": app.user_cache.weighted_size(),
        "balanced_rpcs": app.balanced_rpcs,
        "private_rpcs": app.private_rpcs,
    });

    Json(body)
}
