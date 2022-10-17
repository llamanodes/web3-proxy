//! Used by admins for health checks and inspecting global statistics.
//!
//! For ease of development, users can currently access these endponts.
//! They will eventually move to another port.

use crate::app::Web3ProxyApp;
use axum::{http::StatusCode, response::IntoResponse, Extension, Json};
use moka::future::ConcurrentCacheExt;
use serde_json::json;
use std::sync::Arc;

/// Health check page for load balancers to use.
pub async fn health(Extension(app): Extension<Arc<Web3ProxyApp>>) -> impl IntoResponse {
    // TODO: also check that the head block is not too old
    if app.balanced_rpcs.synced() {
        (StatusCode::OK, "OK")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, ":(")
    }
}

/// Prometheus metrics.
///
/// TODO: when done debugging, remove this and only allow access on a different port
pub async fn prometheus(Extension(app): Extension<Arc<Web3ProxyApp>>) -> impl IntoResponse {
    app.prometheus_metrics()
}

/// Very basic status page.
///
/// TODO: replace this with proper stats and monitoring
pub async fn status(Extension(app): Extension<Arc<Web3ProxyApp>>) -> impl IntoResponse {
    app.pending_transactions.sync();
    app.user_key_cache.sync();

    // TODO: what else should we include? uptime, cache hit rates, cpu load
    let body = json!({
        "pending_transactions_count": app.pending_transactions.entry_count(),
        "pending_transactions_size": app.pending_transactions.weighted_size(),
        "user_cache_count": app.user_key_cache.entry_count(),
        "user_cache_size": app.user_key_cache.weighted_size(),
        "balanced_rpcs": app.balanced_rpcs,
        "private_rpcs": app.private_rpcs,
    });

    Json(body)
}
