use super::errors::FrontendResult;
use super::rate_limit::{rate_limit_by_ip, rate_limit_by_key};
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
    ClientIp(ip): ClientIp,
    Json(payload): Json<JsonRpcRequestEnum>,
    referer: Option<TypedHeader<Referer>>,
    user_agent: Option<TypedHeader<UserAgent>>,
) -> FrontendResult {
    let request_span = error_span!("request", %ip, ?referer, ?user_agent);

    let ip = rate_limit_by_ip(&app, ip)
        .instrument(request_span.clone())
        .await?;

    let f = tokio::spawn(async move { app.proxy_web3_rpc(payload).instrument(request_span).await });

    let response = f.await.unwrap()?;

    Ok(Json(&response).into_response())
}

pub async fn user_proxy_web3_rpc(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
    Json(payload): Json<JsonRpcRequestEnum>,
    referer: Option<TypedHeader<Referer>>,
    user_agent: Option<TypedHeader<UserAgent>>,
    Path(user_key): Path<Uuid>,
) -> FrontendResult {
    let request_span =
        error_span!("request", %ip, ?referer, ?user_agent, user_id = tracing::field::Empty);

    // TODO: this should probably return the user_key_id instead? or maybe both?
    let user_id = rate_limit_by_key(&app, user_key)
        .instrument(request_span.clone())
        .await?;

    request_span.record("user_id", user_id);

    let f = tokio::spawn(async move { app.proxy_web3_rpc(payload).instrument(request_span).await });

    let response = f.await.unwrap()?;

    Ok(Json(&response).into_response())
}
