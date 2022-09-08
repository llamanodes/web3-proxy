use super::errors::FrontendResult;
use super::rate_limit::{rate_limit_by_ip, rate_limit_by_user_key};
use crate::{app::Web3ProxyApp, jsonrpc::JsonRpcRequestEnum};
use axum::extract::{Host, Path};
use axum::headers::{Referer, UserAgent};
use axum::TypedHeader;
use axum::{response::IntoResponse, Extension, Json};
use axum_client_ip::ClientIp;
use std::sync::Arc;
use tracing::{debug_span, error_span, Instrument};
use uuid::Uuid;

pub async fn public_proxy_web3_rpc(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    Host(host): Host,
    ClientIp(ip): ClientIp,
    Json(payload): Json<JsonRpcRequestEnum>,
    referer: Option<TypedHeader<Referer>>,
    user_agent: Option<TypedHeader<UserAgent>>,
) -> FrontendResult {
    let request_span = debug_span!("request", host, ?referer, ?user_agent);

    let ip = rate_limit_by_ip(&app, ip)
        .instrument(request_span.clone())
        .await?;

    let user_span = error_span!("ip", %ip);

    let f = tokio::spawn(async move {
        app.proxy_web3_rpc(payload)
            .instrument(request_span)
            .instrument(user_span)
            .await
    });

    let response = f.await.unwrap()?;

    Ok(Json(&response).into_response())
}

pub async fn user_proxy_web3_rpc(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    Host(host): Host,
    Json(payload): Json<JsonRpcRequestEnum>,
    user_agent: Option<TypedHeader<UserAgent>>,
    Path(user_key): Path<Uuid>,
    referer: Option<TypedHeader<Referer>>,
) -> FrontendResult {
    let request_span = debug_span!("request", host, ?referer, ?user_agent);

    let user_id: u64 = rate_limit_by_user_key(&app, user_key)
        .instrument(request_span.clone())
        .await?;

    let user_span = error_span!("user", user_id);

    let f = tokio::spawn(async move {
        app.proxy_web3_rpc(payload)
            .instrument(request_span)
            .instrument(user_span)
            .await
    });

    let response = f.await.unwrap()?;

    Ok(Json(&response).into_response())
}
