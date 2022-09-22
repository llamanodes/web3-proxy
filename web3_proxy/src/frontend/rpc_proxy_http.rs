use super::authorization::{ip_is_authorized, key_is_authorized};
use super::errors::FrontendResult;
use crate::{app::Web3ProxyApp, jsonrpc::JsonRpcRequestEnum};
use axum::extract::Path;
use axum::headers::{Referer, UserAgent};
use axum::TypedHeader;
use axum::{response::IntoResponse, Extension, Json};
use axum_client_ip::ClientIp;
use std::sync::Arc;
use tracing::{error_span, Instrument};
use uuid::Uuid;

pub async fn public_proxy_web3_rpc(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
    Json(payload): Json<JsonRpcRequestEnum>,
    referer: Option<TypedHeader<Referer>>,
    user_agent: Option<TypedHeader<UserAgent>>,
) -> FrontendResult {
    let request_span = error_span!("request", %ip, ?referer, ?user_agent);

    let authorization = ip_is_authorized(&app, ip)
        .instrument(request_span.clone())
        .await?;

    let request_span = error_span!("request", ?authorization);

    let authorization = Arc::new(authorization);

    let f = tokio::spawn(async move {
        app.proxy_web3_rpc(&authorization, payload)
            .instrument(request_span)
            .await
    });

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
    let request_span = error_span!("request", %ip, ?referer, ?user_agent);

    // TODO: this should probably return the user_key_id instead? or maybe both?
    let authorization = key_is_authorized(
        &app,
        user_key,
        ip,
        referer.map(|x| x.0),
        user_agent.map(|x| x.0),
    )
    .instrument(request_span.clone())
    .await?;

    let request_span = error_span!("request", ?authorization);

    let authorization = Arc::new(authorization);

    let f = tokio::spawn(async move {
        app.proxy_web3_rpc(&authorization, payload)
            .instrument(request_span)
            .await
    });

    let response = f.await.unwrap()?;

    Ok(Json(&response).into_response())
}
