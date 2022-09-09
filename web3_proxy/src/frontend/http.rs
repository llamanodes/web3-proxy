use crate::app::Web3ProxyApp;
use axum::{http::StatusCode, response::IntoResponse, Extension};
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

/// Very basic status page
/// TODO: replace this with proper stats and monitoring
#[instrument(skip_all)]
pub async fn status(Extension(app): Extension<Arc<Web3ProxyApp>>) -> impl IntoResponse {
    // // TODO: what else should we include? uptime?
    // app.pending_transactions.sync();
    // app.user_cache.sync();
    // let body = json!({
    //     "total_queries": app.total_queries.load(atomic::Ordering::Acquire),
    //     "pending_transactions_count": app.pending_transactions.entry_count(),
    //     "pending_transactions_size": app.pending_transactions.weighted_size(),
    //     "user_cache_count": app.user_cache.entry_count(),
    //     "user_cache_size": app.user_cache.weighted_size(),
    //     "balanced_rpcs": app.balanced_rpcs,
    //     "private_rpcs": app.private_rpcs,
    // });
    // Json(body)
    // TODO: only expose this on a different port
    app.prometheus_metrics().unwrap()
}
