//! Used by admins for health checks and inspecting global statistics.
//!
//! For ease of development, users can currently access these endponts.
//! They will eventually move to another port.

use super::{FrontendJsonResponseCache, FrontendResponseCacheKey};
use crate::{
    app::{Web3ProxyApp, APP_USER_AGENT},
    frontend::errors::Web3ProxyError,
};
use axum::{body::Bytes, http::StatusCode, response::IntoResponse, Extension};
use axum_macros::debug_handler;
use futures::Future;
use once_cell::sync::Lazy;
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tokio::time::Instant;

static HEALTH_OK: Lazy<Bytes> = Lazy::new(|| Bytes::from("OK\n"));
static HEALTH_NOT_OK: Lazy<Bytes> = Lazy::new(|| Bytes::from(":(\n"));

static BACKUPS_NEEDED_TRUE: Lazy<Bytes> = Lazy::new(|| Bytes::from("true\n"));
static BACKUPS_NEEDED_FALSE: Lazy<Bytes> = Lazy::new(|| Bytes::from("false\n"));

/// simple ttl for
// TODO: make this generic for any cache/key
async fn _quick_cache_ttl<Fut>(
    app: Arc<Web3ProxyApp>,
    cache: Arc<FrontendJsonResponseCache>,
    key: FrontendResponseCacheKey,
    f: impl Fn(Arc<Web3ProxyApp>) -> Fut,
) -> (StatusCode, Bytes)
where
    Fut: Future<Output = (StatusCode, Bytes)>,
{
    let mut response;
    let expire_at;

    (response, expire_at) = cache
        .get_or_insert_async::<Web3ProxyError>(&key, async {
            let expire_at = Instant::now() + Duration::from_millis(1000);

            let response = f(app.clone()).await;

            Ok((response, expire_at))
        })
        .await
        .unwrap();

    if Instant::now() >= expire_at {
        // TODO: this expiration isn't perfect
        // parallel requests could overwrite eachother
        // its good enough for now
        let expire_at = Instant::now() + Duration::from_millis(1000);

        response = f(app).await;

        cache.insert(key, (response.clone(), expire_at));
    }

    response
}

/// Health check page for load balancers to use.
#[debug_handler]
pub async fn health(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    Extension(cache): Extension<Arc<FrontendJsonResponseCache>>,
) -> impl IntoResponse {
    _quick_cache_ttl(app, cache, FrontendResponseCacheKey::Health, _health).await
}

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
    _quick_cache_ttl(
        app,
        cache,
        FrontendResponseCacheKey::BackupsNeeded,
        _backups_needed,
    )
    .await
}

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
/// TODO: replace this with proper stats and monitoring
#[debug_handler]
pub async fn status(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    Extension(cache): Extension<Arc<FrontendJsonResponseCache>>,
) -> impl IntoResponse {
    _quick_cache_ttl(app, cache, FrontendResponseCacheKey::Status, _status).await
}

// TODO: this doesn't need to be async, but _quick_cache_ttl needs an async function
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
