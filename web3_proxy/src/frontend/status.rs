//! Used by admins for health checks and inspecting global statistics.
//!
//! For ease of development, users can currently access these endponts.
//! They will eventually move to another port.

use super::{FrontendHealthCache, FrontendJsonResponseCache, FrontendResponseCacheKey};
use crate::{
    app::{Web3ProxyApp, APP_USER_AGENT},
    frontend::errors::Web3ProxyError,
};
use axum::{body::Bytes, http::StatusCode, response::IntoResponse, Extension};
use axum_macros::debug_handler;
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tokio::time::Instant;

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

/// Easy alerting if backup servers are in use.
pub async fn backups_needed(Extension(app): Extension<Arc<Web3ProxyApp>>) -> impl IntoResponse {
    let code = {
        let consensus_rpcs = app
            .balanced_rpcs
            .watch_consensus_rpcs_sender
            .borrow()
            .clone();

        if let Some(ref consensus_rpcs) = consensus_rpcs {
            if consensus_rpcs.backups_needed {
                StatusCode::INTERNAL_SERVER_ERROR
            } else {
                StatusCode::OK
            }
        } else {
            // if no consensus, we still "need backups". we just don't have any. which is worse
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };

    if matches!(code, StatusCode::OK) {
        (code, "no backups needed. :)")
    } else {
        (code, "backups needed! :(")
    }
}

/// Very basic status page.
///
/// TODO: replace this with proper stats and monitoring
#[debug_handler]
pub async fn status(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    Extension(response_cache): Extension<Arc<FrontendJsonResponseCache>>,
) -> Bytes {
    let key = FrontendResponseCacheKey::Status;

    let (body, expire_at) = response_cache
        .get_or_insert_async::<Web3ProxyError>(&key, async {
            let expire_at = Instant::now() + Duration::from_millis(100);

            // TODO: what else should we include? uptime, cache hit rates, cpu load, memory used
            // TODO: the hostname is probably not going to change. only get once at the start?
            let body = json!({
                "version": APP_USER_AGENT,
                "chain_id": app.config.chain_id,
                "balanced_rpcs": app.balanced_rpcs,
                "private_rpcs": app.private_rpcs,
                "bundler_4337_rpcs": app.bundler_4337_rpcs,
                "hostname": app.hostname,
            });

            let body = body.to_string().into_bytes();

            let body = Bytes::from(body);

            Ok((body, expire_at))
        })
        .await
        .unwrap();

    if Instant::now() >= expire_at {
        response_cache.remove(&key);
    }

    body
}
