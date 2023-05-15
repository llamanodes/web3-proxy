//! Used by admins for health checks and inspecting global statistics.
//!
//! For ease of development, users can currently access these endponts.
//! They will eventually move to another port.

use super::{FrontendJsonResponseCache, FrontendResponseCacheKey};
use crate::app::{Web3ProxyApp, APP_USER_AGENT};
use axum::{body::Bytes, http::StatusCode, response::IntoResponse, Extension};
use axum_macros::debug_handler;
use once_cell::sync::Lazy;
use serde_json::json;
use std::{convert::Infallible, sync::Arc};

static HEALTH_OK: Lazy<Bytes> = Lazy::new(|| Bytes::from("OK\n"));
static HEALTH_NOT_OK: Lazy<Bytes> = Lazy::new(|| Bytes::from(":(\n"));

static BACKUPS_NEEDED_TRUE: Lazy<Bytes> = Lazy::new(|| Bytes::from("true\n"));
static BACKUPS_NEEDED_FALSE: Lazy<Bytes> = Lazy::new(|| Bytes::from("false\n"));

/// Health check page for load balancers to use.
#[debug_handler]
pub async fn health(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    Extension(cache): Extension<Arc<FrontendJsonResponseCache>>,
) -> impl IntoResponse {
    cache
        .get_or_insert_async::<Infallible, _>(&FrontendResponseCacheKey::Health, async move {
            Ok(_health(app).await)
        })
        .await
}

// TODO: _health doesn't need to be async, but _quick_cache_ttl needs an async function
#[inline]
async fn _health(app: Arc<Web3ProxyApp>) -> (StatusCode, Bytes) {
    if app.balanced_rpcs.synced() {
        (StatusCode::OK, HEALTH_OK.clone())
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, HEALTH_NOT_OK.clone())
    }
}

/// Easy alerting if backup servers are in use.
#[debug_handler]
pub async fn backups_needed(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    Extension(cache): Extension<Arc<FrontendJsonResponseCache>>,
) -> impl IntoResponse {
    cache
        .get_or_insert_async::<Infallible, _>(
            &FrontendResponseCacheKey::BackupsNeeded,
            async move { Ok(_backups_needed(app).await) },
        )
        .await
}

#[inline]
async fn _backups_needed(app: Arc<Web3ProxyApp>) -> (StatusCode, Bytes) {
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
        (code, BACKUPS_NEEDED_FALSE.clone())
    } else {
        (code, BACKUPS_NEEDED_TRUE.clone())
    }
}

/// Very basic status page.
///
/// TODO: replace this with proper stats and monitoring. frontend uses it for their public dashboards though
#[debug_handler]
pub async fn status(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    Extension(cache): Extension<Arc<FrontendJsonResponseCache>>,
) -> impl IntoResponse {
    cache
        .get_or_insert_async::<Infallible, _>(&FrontendResponseCacheKey::Status, async move {
            Ok(_status(app).await)
        })
        .await
}

// TODO: _status doesn't need to be async, but _quick_cache_ttl needs an async function
#[inline]
async fn _status(app: Arc<Web3ProxyApp>) -> (StatusCode, Bytes) {
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

    let code = if app.balanced_rpcs.synced() {
        StatusCode::OK
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };

    (code, body)
}
