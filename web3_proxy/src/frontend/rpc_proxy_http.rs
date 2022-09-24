use super::authorization::{bearer_is_authorized, ip_is_authorized, key_is_authorized};
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
use uuid::Uuid;

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

    let authorization = if let Some(TypedHeader(Authorization(bearer))) = bearer {
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

pub async fn proxy_web3_rpc_with_key(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
    Json(payload): Json<JsonRpcRequestEnum>,
    origin: Option<TypedHeader<Origin>>,
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
        origin.map(|x| x.0),
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
