//! Take a user's HTTP JSON-RPC requests and either respond from local data or proxy the request to a backend rpc server.

use super::authorization::{ip_is_authorized, key_is_authorized};
use super::errors::FrontendResult;
use crate::{app::Web3ProxyApp, jsonrpc::JsonRpcRequestEnum};
use axum::extract::Path;
use axum::headers::authorization::Bearer;
use axum::headers::{Authorization, Origin, Referer, UserAgent};
use axum::TypedHeader;
use axum::{response::IntoResponse, Extension, Json};
use axum_client_ip::ClientIp;
use std::sync::Arc;
use tracing::{error_span, Instrument};

/// POST /rpc -- Public entrypoint for HTTP JSON-RPC requests. Web3 wallets use this.
/// Defaults to rate limiting by IP address, but can also read the Authorization header for a bearer token.
/// If possible, please use a WebSocket instead.
pub async fn proxy_web3_rpc(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    ClientIp(ip): ClientIp,
    Json(payload): Json<JsonRpcRequestEnum>,
    origin: Option<TypedHeader<Origin>>,
    referer: Option<TypedHeader<Referer>>,
    user_agent: Option<TypedHeader<UserAgent>>,
) -> FrontendResult {
    let request_span = error_span!("request", %ip, ?referer, ?user_agent);

    let (authorized_request, _semaphore) = if let Some(TypedHeader(Authorization(bearer))) = bearer
    {
        let origin = origin.map(|x| x.0);
        let referer = referer.map(|x| x.0);
        let user_agent = user_agent.map(|x| x.0);

        bearer_is_authorized(&app, bearer, ip, origin, referer, user_agent)
            .instrument(request_span.clone())
            .await?
    } else {
        ip_is_authorized(&app, ip)
            .instrument(request_span.clone())
            .await?
    };

    let request_span = error_span!("request", ?authorized_request);

    let authorized_request = Arc::new(authorized_request);

    // TODO: spawn earlier?
    let f = tokio::spawn(async move {
        app.proxy_web3_rpc(authorized_request, payload)
            .instrument(request_span)
            .await
    });

    let response = f.await.expect("joinhandle should always work")?;

    Ok(Json(&response).into_response())
}

/// Authenticated entrypoint for HTTP JSON-RPC requests. Web3 wallets use this.
/// Rate limit and billing based on the api key in the url.
/// Can optionally authorized based on origin, referer, or user agent.
/// If possible, please use a WebSocket instead.
pub async fn proxy_web3_rpc_with_key(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
    Json(payload): Json<JsonRpcRequestEnum>,
    origin: Option<TypedHeader<Origin>>,
    referer: Option<TypedHeader<Referer>>,
    user_agent: Option<TypedHeader<UserAgent>>,
    Path(user_key): Path<String>,
) -> FrontendResult {
    let user_key = user_key.parse()?;

    let request_span = error_span!("request", %ip, ?referer, ?user_agent);

    let (authorized_request, _semaphore) = key_is_authorized(
        &app,
        user_key,
        ip,
        origin.map(|x| x.0),
        referer.map(|x| x.0),
        user_agent.map(|x| x.0),
    )
    .instrument(request_span.clone())
    .await?;

    let request_span = error_span!("request", ?authorized_request);

    let authorized_request = Arc::new(authorized_request);

    // the request can take a while, so we spawn so that we can start serving another request
    // TODO: spawn even earlier?
    let f = tokio::spawn(async move {
        app.proxy_web3_rpc(authorized_request, payload)
            .instrument(request_span)
            .await
    });

    let response = f.await.expect("JoinHandle should always work")?;

    Ok(Json(&response).into_response())
}
