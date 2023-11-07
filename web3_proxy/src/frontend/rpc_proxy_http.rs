//! Take a user's HTTP JSON-RPC requests and either respond from local data or proxy the request to a backend rpc server.

use super::authorization::{ip_is_authorized, key_is_authorized};
use super::request_id::RequestId;
use super::rpc_proxy_ws::ProxyMode;
use crate::errors::Web3ProxyError;
use crate::{app::App, jsonrpc::JsonRpcRequestEnum};
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::headers::{Origin, Referer, UserAgent};
use axum::response::Response;
use axum::{response::IntoResponse, Json};
use axum::{Extension, TypedHeader};
use axum_client_ip::InsecureClientIp;
use axum_macros::debug_handler;
use http::HeaderMap;
use itertools::Itertools;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

/// POST /rpc -- Public entrypoint for HTTP JSON-RPC requests. Web3 wallets use this.
/// Defaults to rate limiting by IP address, but can also read the Authorization header for a bearer token.
/// If possible, please use a WebSocket instead.
#[debug_handler]
pub async fn proxy_web3_rpc(
    State(app): State<Arc<App>>,
    InsecureClientIp(ip): InsecureClientIp,
    origin: Option<TypedHeader<Origin>>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    payload: Result<Json<JsonRpcRequestEnum>, JsonRejection>,
) -> Result<Response, Response> {
    _proxy_web3_rpc(
        app,
        &ip,
        origin.as_deref(),
        payload,
        ProxyMode::Best,
        request_id,
    )
    .await
}

#[debug_handler]
pub async fn fastest_proxy_web3_rpc(
    State(app): State<Arc<App>>,
    InsecureClientIp(ip): InsecureClientIp,
    origin: Option<TypedHeader<Origin>>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    payload: Result<Json<JsonRpcRequestEnum>, JsonRejection>,
) -> Result<Response, Response> {
    // TODO: read the fastest number from params
    // TODO: check that the app allows this without authentication
    _proxy_web3_rpc(
        app,
        &ip,
        origin.as_deref(),
        payload,
        ProxyMode::Fastest(0),
        request_id,
    )
    .await
}

#[debug_handler]
pub async fn versus_proxy_web3_rpc(
    State(app): State<Arc<App>>,
    InsecureClientIp(ip): InsecureClientIp,
    origin: Option<TypedHeader<Origin>>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    payload: Result<Json<JsonRpcRequestEnum>, JsonRejection>,
) -> Result<Response, Response> {
    _proxy_web3_rpc(
        app,
        &ip,
        origin.as_deref(),
        payload,
        ProxyMode::Versus,
        request_id,
    )
    .await
}

/// TODO: refactor this to use the builder pattern
async fn _proxy_web3_rpc(
    app: Arc<App>,
    ip: &IpAddr,
    origin: Option<&Origin>,
    payload: Result<Json<JsonRpcRequestEnum>, JsonRejection>,
    proxy_mode: ProxyMode,
    request_id: String,
) -> Result<Response, Response> {
    // TODO: create a stat if they error. (but we haven't parsed rpc_key yet, so it needs some thought)
    let payload = payload
        .map_err(|e| Web3ProxyError::from(e).into_response_with_id(None, None))?
        .0;

    let first_id = payload.first_id();

    let authorization = ip_is_authorized(&app, ip, origin, proxy_mode)
        .await
        .map_err(|e| e.into_response_with_id(first_id.clone(), None))?;

    let authorization = Arc::new(authorization);

    payload
        .tarpit_invalid(&app, &authorization, Duration::from_secs(5))
        .await?;

    // TODO: calculate payload bytes here (before turning into serde_json::Value). that will save serializing later

    // TODO: is first_id the right thing to attach to this error?
    // TODO: i think we want to attach the web3_request here. but that means we need to create it here
    let (status_code, response, rpcs) = app
        .proxy_web3_rpc(authorization, payload, Some(request_id))
        .await
        .map_err(|e| e.into_response_with_id(first_id, None))?;

    let mut response = (status_code, response).into_response();

    // TODO: DRY this up. it is the same code for public and private queries
    let response_headers = response.headers_mut();

    // TODO: this might be slow. think about this more
    // TODO: special string if no rpcs were used (cache hit)?
    let mut backup_used = false;

    let rpcs: String = rpcs
        .into_iter()
        .map(|x| {
            if x.backup {
                backup_used = true;
            }
            x.name.clone()
        })
        .join(",");

    response_headers.insert(
        "X-W3P-BACKEND-RPCS",
        rpcs.parse().expect("W3P-BACKEND-RPCS should always parse"),
    );

    response_headers.insert(
        "X-W3P-BACKUP-RPC",
        backup_used
            .to_string()
            .parse()
            .expect("W3P-BACKEND-RPCS should always parse"),
    );

    Ok(response)
}

/// Authenticated entrypoint for HTTP JSON-RPC requests. Web3 wallets use this.
/// Rate limit and billing based on the api key in the url.
/// Can optionally authorized based on origin, referer, or user agent.
/// If possible, please use a WebSocket instead.
#[debug_handler]
#[allow(clippy::too_many_arguments)]
pub async fn proxy_web3_rpc_with_key(
    State(app): State<Arc<App>>,
    InsecureClientIp(ip): InsecureClientIp,
    origin: Option<TypedHeader<Origin>>,
    referer: Option<TypedHeader<Referer>>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    Path(rpc_key): Path<String>,
    user_agent: Option<TypedHeader<UserAgent>>,
    // body extractors always have to be last
    payload: Result<Json<JsonRpcRequestEnum>, JsonRejection>,
) -> Result<Response, Response> {
    _proxy_web3_rpc_with_key(
        app,
        &ip,
        origin.as_deref(),
        referer.as_deref(),
        user_agent.as_deref(),
        rpc_key,
        payload,
        ProxyMode::Best,
        request_id,
    )
    .await
}

// TODO: if a /debug/ request gets rejected by an invalid request, there won't be any kafka log
#[debug_handler]
#[allow(clippy::too_many_arguments)]
pub async fn debug_proxy_web3_rpc_with_key(
    State(app): State<Arc<App>>,
    InsecureClientIp(ip): InsecureClientIp,
    origin: Option<TypedHeader<Origin>>,
    referer: Option<TypedHeader<Referer>>,
    user_agent: Option<TypedHeader<UserAgent>>,
    request_headers: HeaderMap,
    Path(rpc_key): Path<String>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    // body extractors always have to be last
    payload: Result<Json<JsonRpcRequestEnum>, JsonRejection>,
) -> Result<Response, Response> {
    let mut response = match _proxy_web3_rpc_with_key(
        app,
        &ip,
        origin.as_deref(),
        referer.as_deref(),
        user_agent.as_deref(),
        rpc_key,
        payload,
        ProxyMode::Debug,
        request_id,
    )
    .await
    {
        Ok(r) => r,
        Err(r) => r,
    };

    // add some headers that might be useful while debugging
    let response_headers = response.headers_mut();

    // TODO: move this header name to config
    if let Some(x) = request_headers.get("x-amzn-trace-id").cloned() {
        response_headers.insert("x-amzn-trace-id", x);
    }

    if let Some(x) = request_headers.get("x-balance-id").cloned() {
        response_headers.insert("x-balance-id", x);
    }

    response_headers.insert("client-ip", ip.to_string().parse().unwrap());

    Ok(response)
}

#[debug_handler]
#[allow(clippy::too_many_arguments)]
pub async fn fastest_proxy_web3_rpc_with_key(
    State(app): State<Arc<App>>,
    InsecureClientIp(ip): InsecureClientIp,
    origin: Option<TypedHeader<Origin>>,
    referer: Option<TypedHeader<Referer>>,
    Path(rpc_key): Path<String>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    user_agent: Option<TypedHeader<UserAgent>>,
    // body extractors always have to be last
    payload: Result<Json<JsonRpcRequestEnum>, JsonRejection>,
) -> Result<Response, Response> {
    _proxy_web3_rpc_with_key(
        app,
        &ip,
        origin.as_deref(),
        referer.as_deref(),
        user_agent.as_deref(),
        rpc_key,
        payload,
        ProxyMode::Fastest(0),
        request_id,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
#[debug_handler]
pub async fn versus_proxy_web3_rpc_with_key(
    State(app): State<Arc<App>>,
    InsecureClientIp(ip): InsecureClientIp,
    origin: Option<TypedHeader<Origin>>,
    referer: Option<TypedHeader<Referer>>,
    user_agent: Option<TypedHeader<UserAgent>>,
    Path(rpc_key): Path<String>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    payload: Result<Json<JsonRpcRequestEnum>, JsonRejection>,
) -> Result<Response, Response> {
    _proxy_web3_rpc_with_key(
        app,
        &ip,
        origin.as_deref(),
        referer.as_deref(),
        user_agent.as_deref(),
        rpc_key,
        payload,
        ProxyMode::Versus,
        request_id,
    )
    .await
}

/// TODO: refactor this to use the builder pattern
#[allow(clippy::too_many_arguments)]
async fn _proxy_web3_rpc_with_key(
    app: Arc<App>,
    ip: &IpAddr,
    origin: Option<&Origin>,
    referer: Option<&Referer>,
    user_agent: Option<&UserAgent>,
    rpc_key: String,
    payload: Result<Json<JsonRpcRequestEnum>, JsonRejection>,
    proxy_mode: ProxyMode,
    request_id: String,
) -> Result<Response, Response> {
    // TODO: DRY w/ proxy_web3_rpc
    // TODO: create a stat if they error. (but we haven't parsed rpc_key yet, so it needs some thought)
    let payload = payload
        .map_err(|e| Web3ProxyError::from(e).into_response_with_id(None, None))?
        .0;

    let first_id = payload.first_id();

    let rpc_key = rpc_key
        .parse()
        .map_err(|e: Web3ProxyError| e.into_response_with_id(first_id.clone(), None))?;

    let authorization =
        key_is_authorized(&app, &rpc_key, ip, origin, proxy_mode, referer, user_agent)
            .await
            .map_err(|e| e.into_response_with_id(first_id.clone(), None))?;

    let authorization = Arc::new(authorization);

    payload
        .tarpit_invalid(&app, &authorization, Duration::from_secs(2))
        .await?;

    let rpc_secret_key_id = authorization.checks.rpc_secret_key_id;

    // TODO: pass web3_request to the map_err
    let (status_code, response, rpcs) = app
        .proxy_web3_rpc(authorization, payload, Some(request_id))
        .await
        .map_err(|e| e.into_response_with_id(first_id, None))?;

    let mut response = (status_code, response).into_response();

    let headers = response.headers_mut();

    let mut backup_used = false;

    // TODO: special string if no rpcs were used (cache hit)? or is an empty string fine? maybe the rpc name + "cached"
    let rpcs: String = rpcs
        .into_iter()
        .map(|x| {
            if x.backup {
                backup_used = true;
            }
            x.name.clone()
        })
        .join(",");

    headers.insert(
        "X-W3P-BACKEND-RPCs",
        rpcs.parse().expect("W3P-BACKEND-RPCS should always parse"),
    );

    headers.insert(
        "X-W3P-BACKUP-RPC",
        backup_used
            .to_string()
            .parse()
            .expect("W3P-BACKEND-RPCS should always parse"),
    );

    if let Some(rpc_secret_key_id) = rpc_secret_key_id {
        headers.insert(
            "X-W3P-KEY-ID",
            rpc_secret_key_id
                .to_string()
                .parse()
                .expect("X-CLIENT-IP should always parse"),
        );
    }

    // TODO: user tier in the header

    Ok(response)
}
