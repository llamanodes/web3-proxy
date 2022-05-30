/// this should move into web3-proxy once the basics are working
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    handler::Handler,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Extension, Json, Router,
};
use futures::stream::{SplitSink, SplitStream, StreamExt};
use futures::SinkExt;
use hashbrown::HashMap;
use serde_json::json;
use serde_json::value::RawValue;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{error, warn};

use crate::{
    app::Web3ProxyApp,
    jsonrpc::{
        JsonRpcForwardedResponse, JsonRpcForwardedResponseEnum, JsonRpcRequest, JsonRpcRequestEnum,
    },
};

pub async fn run(port: u16, proxy_app: Arc<Web3ProxyApp>) -> anyhow::Result<()> {
    // build our application with a route
    let app = Router::new()
        // `GET /` goes to `root`
        .route("/", get(root))
        // `POST /` goes to `proxy_web3_rpc`
        .route("/", post(proxy_web3_rpc))
        // `websocket /` goes to `proxy_web3_ws`
        .route("/ws", get(websocket_handler))
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

async fn websocket_handler(
    app: Extension<Arc<Web3ProxyApp>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| proxy_web3_socket(app, socket))
}

async fn proxy_web3_socket(app: Extension<Arc<Web3ProxyApp>>, socket: WebSocket) {
    // split the websocket so we can read and write concurrently
    let (ws_tx, ws_rx) = socket.split();

    // create a channel for our reader and writer can communicate. todo: benchmark different channels
    let (response_tx, response_rx) = flume::unbounded::<Message>();

    tokio::spawn(write_web3_socket(response_rx, ws_tx));
    tokio::spawn(read_web3_socket(app, ws_rx, response_tx));
}

async fn read_web3_socket(
    app: Extension<Arc<Web3ProxyApp>>,
    mut ws_rx: SplitStream<WebSocket>,
    response_tx: flume::Sender<Message>,
) {
    let mut subscriptions = HashMap::new();

    while let Some(Ok(msg)) = ws_rx.next().await {
        // new message from our client. forward to a backend and then send it through response_tx
        let response_msg = match msg {
            Message::Text(payload) => {
                let (id, response) = match serde_json::from_str::<JsonRpcRequest>(&payload) {
                    Ok(payload) => {
                        let id = payload.id.clone();

                        let response: anyhow::Result<JsonRpcForwardedResponseEnum> =
                            if payload.method == "eth_subscribe" {
                                // TODO: if we pass eth_subscribe the response_tx, we
                                let response = app
                                    .0
                                    .eth_subscribe(id.clone(), payload, response_tx.clone())
                                    .await;

                                match response {
                                    Ok((handle, response)) => {
                                        // TODO: better key
                                        subscriptions.insert(
                                            response.result.as_ref().unwrap().to_string(),
                                            handle,
                                        );

                                        Ok(response.into())
                                    }
                                    Err(err) => Err(err),
                                }
                            } else if payload.method == "eth_unsubscribe" {
                                let subscription_id = payload.params.unwrap().to_string();

                                let partial_response = match subscriptions.remove(&subscription_id)
                                {
                                    None => "false",
                                    Some(handle) => {
                                        handle.abort();
                                        "true"
                                    }
                                };

                                let response = JsonRpcForwardedResponse::from_string(
                                    partial_response.to_string(),
                                    id.clone(),
                                );

                                Ok(response.into())
                            } else {
                                // TODO: if this is a subscription request, we need to do some special handling. something with channels
                                // TODO: just handle subscribe_newBlock

                                app.0.proxy_web3_rpc(payload.into()).await
                            };

                        (id, response)
                    }
                    Err(err) => {
                        // TODO: what should this id be?
                        let id = RawValue::from_string("0".to_string()).unwrap();
                        (id, Err(err.into()))
                    }
                };

                let response_str = match response {
                    Ok(x) => serde_json::to_string(&x),
                    Err(err) => {
                        // we have an anyhow error. turn it into
                        let response = JsonRpcForwardedResponse::from_anyhow_error(err, id);
                        serde_json::to_string(&response)
                    }
                }
                .unwrap();

                Message::Text(response_str)
            }
            Message::Ping(x) => Message::Pong(x),
            _ => unimplemented!(),
        };

        match response_tx.send_async(response_msg).await {
            Ok(_) => {}
            Err(err) => {
                error!("{}", err);
                break;
            }
        };
    }
}

async fn write_web3_socket(
    response_rx: flume::Receiver<Message>,
    mut ws_tx: SplitSink<WebSocket, Message>,
) {
    while let Ok(msg) = response_rx.recv_async().await {
        // a response is ready. write it to ws_tx
        if ws_tx.send(msg).await.is_err() {
            // TODO: log the error
            break;
        };
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
    // TODO: what id can we use? how do we make sure the incoming id gets attached to this?
    let id = RawValue::from_string("0".to_string()).unwrap();

    let err = JsonRpcForwardedResponse::from_anyhow_error(err, id);

    warn!("Responding with error: {:?}", err);

    let code = code.unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    (code, Json(err))
}

// i think we want a custom result type. it has an anyhow result inside. it impl IntoResponse
