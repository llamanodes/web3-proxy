use crate::app::Web3ProxyApp;
use axum::{http::StatusCode, response::IntoResponse, Extension, Json};
use moka::future::ConcurrentCacheExt;
use serde_json::json;
use std::sync::Arc;
use tracing::instrument;

/// Health check page for load balancers to use
#[instrument(skip_all)]
pub async fn health(Extension(app): Extension<Arc<Web3ProxyApp>>) -> impl IntoResponse {
    if app.balanced_rpcs.synced() {
        (StatusCode::OK, "OK")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, ":(")
    }
}

/// Prometheus metrics
/// TODO: when done debugging, remove this and only allow access on a different port
#[instrument(skip_all)]
pub async fn prometheus(Extension(app): Extension<Arc<Web3ProxyApp>>) -> impl IntoResponse {
    app.prometheus_metrics()
}

/// Very basic status page
/// TODO: replace this with proper stats and monitoring
#[instrument(skip_all)]
pub async fn status(Extension(app): Extension<Arc<Web3ProxyApp>>) -> impl IntoResponse {
    // TODO: what else should we include? uptime?
    app.pending_transactions.sync();
    app.user_cache.sync();

    let body = json!({
        "pending_transactions_count": app.pending_transactions.entry_count(),
        "pending_transactions_size": app.pending_transactions.weighted_size(),
        "user_cache_count": app.user_cache.entry_count(),
        "user_cache_size": app.user_cache.weighted_size(),
        "balanced_rpcs": app.balanced_rpcs,
        "private_rpcs": app.private_rpcs,
    });

    Json(body)
}
