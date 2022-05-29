/// this should move into web3-proxy once the basics are working
use axum::{
    // error_handling::HandleError,
    handler::Handler,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Extension,
    Json,
    Router,
};
use serde_json::json;
use serde_json::value::RawValue;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::warn;

use crate::{
    app::Web3ProxyApp,
    jsonrpc::{JsonRpcErrorData, JsonRpcForwardedResponse, JsonRpcRequestEnum},
};

pub async fn run(port: u16, proxy_app: Arc<Web3ProxyApp>) -> anyhow::Result<()> {
    // build our application with a route
    let app = Router::new()
        // `GET /` goes to `root`
        .route("/", get(root))
        // `POST /` goes to `proxy_web3_rpc`
        .route("/", post(proxy_web3_rpc))
        // `GET /status` goes to `status`
        .route("/status", get(status))
        .layer(Extension(proxy_app));

    // 404 for any unknown routes
    let app = app.fallback(handler_404.into_service());

    // run our app with hyper
    // `axum::Server` is a re-export of `hyper::Server`
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("listening on port {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .map_err(Into::into)
}

/// a page for configuring your wallet with all the rpcs
/// TODO: check auth (from authp?) here
async fn root() -> impl IntoResponse {
    "Hello, World!"
}

/// TODO: check auth (from authp?) here
async fn proxy_web3_rpc(
    payload: Json<JsonRpcRequestEnum>,
    app: Extension<Arc<Web3ProxyApp>>,
) -> impl IntoResponse {
    match app.0.proxy_web3_rpc(payload.0).await {
        Ok(response) => (StatusCode::OK, Json(&response)).into_response(),
        Err(err) => _handle_anyhow_error(err, None).await.into_response(),
    }
}

/// Very basic status page
async fn status(app: Extension<Arc<Web3ProxyApp>>) -> impl IntoResponse {
    let app = app.0.as_ref();

    let balanced_rpcs = app.get_balanced_rpcs();

    let private_rpcs = app.get_private_rpcs();

    let num_active_requests = app.get_active_requests().len();

    // TODO: what else should we include? uptime? prometheus?
    let body = json!({
        "balanced_rpcs": balanced_rpcs,
        "private_rpcs": private_rpcs,
        "num_active_requests": num_active_requests,
    });

    (StatusCode::INTERNAL_SERVER_ERROR, Json(body))
}

/// TODO: pretty 404 page? or us a json error fine?
async fn handler_404() -> impl IntoResponse {
    let err = anyhow::anyhow!("nothing to see here");

    _handle_anyhow_error(err, Some(StatusCode::NOT_FOUND)).await
}

/// handle errors by converting them into something that implements `IntoResponse`
/// TODO: use this. i can't get https://docs.rs/axum/latest/axum/error_handling/index.html to work
async fn _handle_anyhow_error(err: anyhow::Error, code: Option<StatusCode>) -> impl IntoResponse {
    let err = format!("{:?}", err);

    warn!("Responding with error: {}", err);

    let err = JsonRpcForwardedResponse {
        jsonrpc: "2.0".to_string(),
        // TODO: what id can we use? how do we make sure the incoming id gets attached to this?
        id: RawValue::from_string("0".to_string()).unwrap(),
        result: None,
        error: Some(JsonRpcErrorData {
            code: -32099,
            message: err,
            data: None,
        }),
    };

    let code = code.unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    (code, Json(err))
}

// i think we want a custom result type. it has an anyhow result inside. it impl IntoResponse
