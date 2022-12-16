//! Used by admins for health checks and inspecting global statistics.
//!
//! For ease of development, users can currently access these endponts.
//! They will eventually move to another port.

use super::{FrontendResponseCache, FrontendResponseCaches};
use crate::app::Web3ProxyApp;
use axum::{http::StatusCode, response::IntoResponse, Extension, Json};
use axum_macros::debug_handler;
use moka::future::ConcurrentCacheExt;
use serde_json::json;
use std::sync::Arc;

/// Health check page for load balancers to use.
#[debug_handler]
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
#[debug_handler]
pub async fn prometheus(Extension(app): Extension<Arc<Web3ProxyApp>>) -> impl IntoResponse {
    app.prometheus_metrics()
}

/// Very basic status page.
///
/// TODO: replace this with proper stats and monitoring
#[debug_handler]
pub async fn status(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    Extension(response_cache): Extension<FrontendResponseCache>,
) -> impl IntoResponse {
    let body = response_cache
        .get_with(FrontendResponseCaches::Status, async {
            app.pending_transactions.sync();
            app.rpc_secret_key_cache.sync();

            // TODO: what else should we include? uptime, cache hit rates, cpu load
            let body = json!({
                "chain_id": app.config.chain_id,
                "pending_transactions_count": app.pending_transactions.entry_count(),
                "pending_transactions_size": app.pending_transactions.weighted_size(),
                "user_cache_count": app.rpc_secret_key_cache.entry_count(),
                "user_cache_size": app.rpc_secret_key_cache.weighted_size(),
                "balanced_rpcs": app.balanced_rpcs,
                "private_rpcs": app.private_rpcs,
            });

            Arc::new(body)
        })
        .await;

    Json(body)
}
