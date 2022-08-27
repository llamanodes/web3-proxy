use super::errors::FrontendResult;
use super::rate_limit::{rate_limit_by_ip, rate_limit_by_user_key};
use crate::stats::Protocol;
use crate::{app::Web3ProxyApp, jsonrpc::JsonRpcRequestEnum};
use axum::extract::Path;
use axum::{http::StatusCode, response::IntoResponse, Extension, Json};
use axum_client_ip::ClientIp;
use std::sync::Arc;
use tracing::{error_span, Instrument};
use uuid::Uuid;

pub async fn public_proxy_web3_rpc(
    Json(payload): Json<JsonRpcRequestEnum>,
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
) -> FrontendResult {
    let _ip = rate_limit_by_ip(&app, ip).await?;

    let protocol = Protocol::HTTP;
    let user_id = 0;

    let user_span = error_span!("user", user_id, ?protocol);

    /*
    // TODO: move this to a helper function (or two). have it fetch method, protocol, etc. from tracing?
    match &payload {
        JsonRpcRequestEnum::Batch(batch) => {
            // TODO: use inc_by if possible? need to group them by rpc_method
            for single in batch {
                let rpc_method = single.method.clone();

                let _count = app
                    .stats
                    .proxy_requests
                    .get_or_create(&ProxyRequestLabels {
                        rpc_method,
                        protocol: protocol.clone(),
                        user_id,
                    })
                    .inc();
            }
        }
        JsonRpcRequestEnum::Single(single) => {
            let rpc_method = single.method.clone();

            let _count = app
                .stats
                .proxy_requests
                .get_or_create(&ProxyRequestLabels {
                    protocol,
                    rpc_method,
                    user_id,
                })
                .inc();
        }
    };
    */

    let response = app.proxy_web3_rpc(payload).instrument(user_span).await?;

    Ok((StatusCode::OK, Json(&response)).into_response())
}

pub async fn user_proxy_web3_rpc(
    Json(payload): Json<JsonRpcRequestEnum>,
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    Path(user_key): Path<Uuid>,
) -> FrontendResult {
    let user_id: u64 = rate_limit_by_user_key(&app, user_key).await?;

    let protocol = Protocol::HTTP;

    let user_span = error_span!("user", user_id, ?protocol);

    let response = app.proxy_web3_rpc(payload).instrument(user_span).await?;

    Ok((StatusCode::OK, Json(&response)).into_response())
}
