use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    Extension,
};
use futures::SinkExt;
use futures::{
    future::AbortHandle,
    stream::{SplitSink, SplitStream, StreamExt},
};
use hashbrown::HashMap;
use serde_json::value::RawValue;
use std::str::from_utf8_mut;
use std::sync::Arc;
use tracing::{error, info, trace, warn};

use crate::{
    app::Web3ProxyApp,
    jsonrpc::{JsonRpcForwardedResponse, JsonRpcForwardedResponseEnum, JsonRpcRequest},
};

pub async fn websocket_handler(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| proxy_web3_socket(app, socket))
}

async fn proxy_web3_socket(app: Arc<Web3ProxyApp>, socket: WebSocket) {
    // split the websocket so we can read and write concurrently
    let (ws_tx, ws_rx) = socket.split();

    // create a channel for our reader and writer can communicate. todo: benchmark different channels
    let (response_tx, response_rx) = flume::unbounded::<Message>();

    tokio::spawn(write_web3_socket(response_rx, ws_tx));
    tokio::spawn(read_web3_socket(app, ws_rx, response_tx));
}

async fn handle_socket_payload(
    app: Arc<Web3ProxyApp>,
    payload: &str,
    response_tx: &flume::Sender<Message>,
    subscriptions: &mut HashMap<String, AbortHandle>,
) -> Message {
    let (id, response) = match serde_json::from_str::<JsonRpcRequest>(payload) {
        Ok(payload) => {
            let id = payload.id.clone();

            let response: anyhow::Result<JsonRpcForwardedResponseEnum> = match &payload.method[..] {
                "eth_subscribe" => {
                    let response = app
                        .clone()
                        .eth_subscribe(payload, response_tx.clone())
                        .await;

                    match response {
                        Ok((handle, response)) => {
                            // TODO: better key
                            subscriptions
                                .insert(response.result.as_ref().unwrap().to_string(), handle);

                            Ok(response.into())
                        }
                        Err(err) => Err(err),
                    }
                }
                "eth_unsubscribe" => {
                    let subscription_id = payload.params.unwrap().to_string();

                    let partial_response = match subscriptions.remove(&subscription_id) {
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
                }
                _ => app.proxy_web3_rpc(payload.into()).await,
            };

            (id, response)
        }
        Err(err) => {
            // TODO: what should this id be?
            let id = RawValue::from_string("null".to_string()).unwrap();
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

async fn read_web3_socket(
    app: Arc<Web3ProxyApp>,
    mut ws_rx: SplitStream<WebSocket>,
    response_tx: flume::Sender<Message>,
) {
    let mut subscriptions = HashMap::new();

    while let Some(Ok(msg)) = ws_rx.next().await {
        // new message from our client. forward to a backend and then send it through response_tx
        let response_msg = match msg {
            Message::Text(payload) => {
                handle_socket_payload(app.clone(), &payload, &response_tx, &mut subscriptions).await
            }
            Message::Ping(x) => Message::Pong(x),
            Message::Pong(x) => {
                trace!("pong: {:?}", x);
                continue;
            }
            Message::Close(_) => {
                info!("closing websocket connection");
                break;
            }
            Message::Binary(mut payload) => {
                let payload = from_utf8_mut(&mut payload).unwrap();

                handle_socket_payload(app.clone(), payload, &response_tx, &mut subscriptions).await
            }
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
        if let Err(err) = ws_tx.send(msg).await {
            warn!(?err, "unable to write to websocket");
            break;
        };
    }
}
