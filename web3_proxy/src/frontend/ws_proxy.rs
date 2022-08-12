use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::Path,
    response::{IntoResponse, Redirect, Response},
    Extension,
};
use axum_client_ip::ClientIp;
use fstrings::{format_args_f, format_f};
use futures::SinkExt;
use futures::{
    future::AbortHandle,
    stream::{SplitSink, SplitStream, StreamExt},
};
use hashbrown::HashMap;
use serde_json::{json, value::RawValue};
use std::sync::Arc;
use std::{str::from_utf8_mut, sync::atomic::AtomicUsize};
use tracing::{error, info, trace};
use uuid::Uuid;

use crate::{
    app::Web3ProxyApp,
    jsonrpc::{JsonRpcForwardedResponse, JsonRpcForwardedResponseEnum, JsonRpcRequest},
};

use super::{errors::anyhow_error_into_response, rate_limit::RateLimitResult};

pub async fn public_websocket_handler(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
    ws_upgrade: Option<WebSocketUpgrade>,
) -> Response {
    let ip = match app.rate_limit_by_ip(ip).await {
        Ok(x) => match x.try_into_response().await {
            Ok(RateLimitResult::AllowedIp(x)) => x,
            Err(err_response) => return err_response,
            _ => unimplemented!(),
        },
        Err(err) => return anyhow_error_into_response(None, None, err).into_response(),
    };

    match ws_upgrade {
        Some(ws) => ws
            .on_upgrade(|socket| proxy_web3_socket(app, socket))
            .into_response(),
        None => {
            // this is not a websocket. give a friendly page. maybe redirect to the llama nodes home
            // TODO: redirect to a configurable url
            Redirect::to(&format_f!(
                "https://llamanodes.com/free-rpc-stats#userip={ip}"
            ))
            .into_response()
        }
    }
}

pub async fn user_websocket_handler(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    Path(user_key): Path<Uuid>,
    ws_upgrade: Option<WebSocketUpgrade>,
) -> Response {
    // TODO: dry this up. maybe a rate_limit_by_key_response function?
    let user_id = match app.rate_limit_by_key(user_key).await {
        Ok(x) => match x.try_into_response().await {
            Ok(RateLimitResult::AllowedUser(x)) => x,
            Err(err_response) => return err_response,
            _ => unimplemented!(),
        },
        Err(err) => return anyhow_error_into_response(None, None, err).into_response(),
    };

    match ws_upgrade {
        Some(ws_upgrade) => ws_upgrade.on_upgrade(|socket| proxy_web3_socket(app, socket)),
        None => {
            // this is not a websocket. give a friendly page with stats for this user
            // TODO: redirect to a configurable url
            // TODO: query to get the user's address. expose that instead of user_id
            Redirect::to(&format_f!(
                "https://llamanodes.com/user-rpc-stats#user_id={user_id}"
            ))
            .into_response()
        }
    }
}

async fn proxy_web3_socket(app: Arc<Web3ProxyApp>, socket: WebSocket) {
    // split the websocket so we can read and write concurrently
    let (ws_tx, ws_rx) = socket.split();

    // create a channel for our reader and writer can communicate. todo: benchmark different channels
    let (response_tx, response_rx) = flume::unbounded::<Message>();

    tokio::spawn(write_web3_socket(response_rx, ws_tx));
    tokio::spawn(read_web3_socket(app, ws_rx, response_tx));
}

/// websockets support a few more methods than http clients
async fn handle_socket_payload(
    app: Arc<Web3ProxyApp>,
    payload: &str,
    response_sender: &flume::Sender<Message>,
    subscription_count: &AtomicUsize,
    subscriptions: &mut HashMap<String, AbortHandle>,
) -> Message {
    // TODO: do any clients send batches over websockets?
    let (id, response) = match serde_json::from_str::<JsonRpcRequest>(payload) {
        Ok(payload) => {
            let id = payload.id.clone();

            let response: anyhow::Result<JsonRpcForwardedResponseEnum> = match &payload.method[..] {
                "eth_subscribe" => {
                    let response = app
                        .clone()
                        .eth_subscribe(payload, subscription_count, response_sender.clone())
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

                    let response =
                        JsonRpcForwardedResponse::from_value(json!(partial_response), id.clone());

                    Ok(response.into())
                }
                _ => app.proxy_web3_rpc(payload.into()).await,
            };

            (id, response)
        }
        Err(err) => {
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
    response_sender: flume::Sender<Message>,
) {
    let mut subscriptions = HashMap::new();
    let subscription_count = AtomicUsize::new(1);

    while let Some(Ok(msg)) = ws_rx.next().await {
        // new message from our client. forward to a backend and then send it through response_tx
        let response_msg = match msg {
            Message::Text(payload) => {
                handle_socket_payload(
                    app.clone(),
                    &payload,
                    &response_sender,
                    &subscription_count,
                    &mut subscriptions,
                )
                .await
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
                // TODO: poke rate limit for the user/ip
                let payload = from_utf8_mut(&mut payload).unwrap();

                handle_socket_payload(
                    app.clone(),
                    payload,
                    &response_sender,
                    &subscription_count,
                    &mut subscriptions,
                )
                .await
            }
        };

        match response_sender.send_async(response_msg).await {
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
    // TODO: increment counter for open websockets

    while let Ok(msg) = response_rx.recv_async().await {
        // a response is ready

        // TODO: poke rate limits for this user?

        // forward the response to through the websocket
        if let Err(err) = ws_tx.send(msg).await {
            // this isn't a problem. this is common and happens whenever a client disconnects
            trace!(?err, "unable to write to websocket");
            break;
        };
    }

    // TODO: decrement counter for open websockets
}
