//! Used by admins for health checks and inspecting global statistics.
//!
//! For ease of development, users can currently access these endponts.
//! They will eventually move to another port.

use super::{FrontendHealthCache, FrontendResponseCache, FrontendResponseCaches};
use crate::app::{Web3ProxyApp, APP_USER_AGENT};
use axum::{http::StatusCode, response::IntoResponse, Extension, Json};
use axum_macros::debug_handler;
use serde_json::json;
use std::sync::Arc;

/// Health check page for load balancers to use.
#[debug_handler]
pub async fn health(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    Extension(health_cache): Extension<FrontendHealthCache>,
) -> impl IntoResponse {
    let synced = health_cache
        .get_with((), async { app.balanced_rpcs.synced() })
        .await;

    if synced {
        (StatusCode::OK, "OK")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, ":(")
    }
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
            // TODO: what else should we include? uptime, cache hit rates, cpu load, memory used
            let body = json!({
                "version": APP_USER_AGENT,
                "chain_id": app.config.chain_id,
                "balanced_rpcs": app.balanced_rpcs,
                "private_rpcs": app.private_rpcs,
            });

            Arc::new(body)
        })
        .await;

    Json(body)
}
