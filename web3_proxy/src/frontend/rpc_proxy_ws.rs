//! Take a user's WebSocket JSON-RPC requests and either respond from local data or proxy the request to a backend rpc server.
//!
//! WebSockets are the preferred method of receiving requests, but not all clients have good support.

use super::authorization::{ip_is_authorized, key_is_authorized, Authorization, RequestMetadata};
use super::errors::{FrontendErrorResponse, FrontendResult};
use crate::app::REQUEST_PERIOD;
use crate::app_stats::ProxyResponseStat;
use crate::{
    app::Web3ProxyApp,
    jsonrpc::{JsonRpcForwardedResponse, JsonRpcForwardedResponseEnum, JsonRpcRequest},
};
use axum::headers::{Origin, Referer, UserAgent};
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::Path,
    response::{IntoResponse, Redirect},
    Extension, TypedHeader,
};
use axum_client_ip::ClientIp;
use axum_macros::debug_handler;
use futures::SinkExt;
use futures::{
    future::AbortHandle,
    stream::{SplitSink, SplitStream, StreamExt},
};
use handlebars::Handlebars;
use hashbrown::HashMap;
use http::StatusCode;
use log::{error, info, trace, warn};
use serde_json::{json, value::RawValue};
use std::sync::Arc;
use std::{str::from_utf8_mut, sync::atomic::AtomicUsize};

/// Public entrypoint for WebSocket JSON-RPC requests.
#[debug_handler]
pub async fn websocket_handler(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
    origin: Option<TypedHeader<Origin>>,
    ws_upgrade: Option<WebSocketUpgrade>,
) -> FrontendResult {
    let origin = origin.map(|x| x.0);

    let (authorization, _semaphore) = ip_is_authorized(&app, ip, origin).await?;

    let authorization = Arc::new(authorization);

    match ws_upgrade {
        Some(ws) => Ok(ws
            .on_upgrade(|socket| proxy_web3_socket(app, authorization, socket))
            .into_response()),
        None => {
            if let Some(redirect) = &app.config.redirect_public_url {
                // this is not a websocket. redirect to a friendly page
                Ok(Redirect::permanent(redirect).into_response())
            } else {
                // TODO: do not use an anyhow error. send the user a 400
                Err(
                    anyhow::anyhow!("redirect_public_url not set. only websockets work here")
                        .into(),
                )
            }
        }
    }
}

/// Authenticated entrypoint for WebSocket JSON-RPC requests. Web3 wallets use this.
/// Rate limit and billing based on the api key in the url.
/// Can optionally authorized based on origin, referer, or user agent.
#[debug_handler]
pub async fn websocket_handler_with_key(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
    Path(rpc_key): Path<String>,
    origin: Option<TypedHeader<Origin>>,
    referer: Option<TypedHeader<Referer>>,
    user_agent: Option<TypedHeader<UserAgent>>,
    ws_upgrade: Option<WebSocketUpgrade>,
) -> FrontendResult {
    let rpc_key = rpc_key.parse()?;

    let (authorization, _semaphore) = key_is_authorized(
        &app,
        rpc_key,
        ip,
        origin.map(|x| x.0),
        referer.map(|x| x.0),
        user_agent.map(|x| x.0),
    )
    .await?;

    trace!("websocket_handler_with_key {:?}", authorization);

    let authorization = Arc::new(authorization);

    match ws_upgrade {
        Some(ws_upgrade) => {
            Ok(ws_upgrade.on_upgrade(move |socket| proxy_web3_socket(app, authorization, socket)))
        }
        None => {
            // if no websocket upgrade, this is probably a user loading the url with their browser

            // TODO: rate limit here? key_is_authorized might be enough

            match (
                &app.config.redirect_public_url,
                &app.config.redirect_rpc_key_url,
                authorization.checks.rpc_key_id,
            ) {
                (None, None, _) => Err(FrontendErrorResponse::StatusCode(
                    StatusCode::BAD_REQUEST,
                    "this page is for rpcs".to_string(),
                    None,
                )),
                (Some(redirect_public_url), _, None) => {
                    Ok(Redirect::permanent(redirect_public_url).into_response())
                }
                (_, Some(redirect_rpc_key_url), rpc_key_id) => {
                    let reg = Handlebars::new();

                    if authorization.checks.rpc_key_id.is_none() {
                        // i don't think this is possible
                        Err(FrontendErrorResponse::StatusCode(
                            StatusCode::UNAUTHORIZED,
                            "AUTHORIZATION header required".to_string(),
                            None,
                        ))
                    } else {
                        let redirect_rpc_key_url = reg
                            .render_template(
                                redirect_rpc_key_url,
                                &json!({ "rpc_key_id": rpc_key_id }),
                            )
                            .expect("templating should always work");

                        // this is not a websocket. redirect to a page for this user
                        Ok(Redirect::permanent(&redirect_rpc_key_url).into_response())
                    }
                }
                // any other combinations get a simple error
                _ => Err(FrontendErrorResponse::StatusCode(
                    StatusCode::BAD_REQUEST,
                    "this page is for rpcs".to_string(),
                    None,
                )),
            }
        }
    }
}

async fn proxy_web3_socket(
    app: Arc<Web3ProxyApp>,
    authorization: Arc<Authorization>,
    socket: WebSocket,
) {
    // split the websocket so we can read and write concurrently
    let (ws_tx, ws_rx) = socket.split();

    // create a channel for our reader and writer can communicate. todo: benchmark different channels
    let (response_sender, response_receiver) = flume::unbounded::<Message>();

    tokio::spawn(write_web3_socket(response_receiver, ws_tx));
    tokio::spawn(read_web3_socket(app, authorization, ws_rx, response_sender));
}

/// websockets support a few more methods than http clients

async fn handle_socket_payload(
    app: Arc<Web3ProxyApp>,
    authorization: &Arc<Authorization>,
    payload: &str,
    response_sender: &flume::Sender<Message>,
    subscription_count: &AtomicUsize,
    subscriptions: &mut HashMap<String, AbortHandle>,
) -> Message {
    // TODO: do any clients send batches over websockets?
    let (id, response) = match serde_json::from_str::<JsonRpcRequest>(payload) {
        Ok(json_request) => {
            let id = json_request.id.clone();

            let response: anyhow::Result<JsonRpcForwardedResponseEnum> = match &json_request.method
                [..]
            {
                "eth_subscribe" => {
                    match app
                        .eth_subscribe(
                            authorization.clone(),
                            json_request,
                            subscription_count,
                            response_sender.clone(),
                        )
                        .await
                    {
                        Ok((handle, response)) => {
                            // TODO: better key
                            subscriptions.insert(
                                response
                                    .result
                                    .as_ref()
                                    // TODO: what if there is an error?
                                    .expect("response should always have a result, not an error")
                                    .to_string(),
                                handle,
                            );

                            Ok(response.into())
                        }
                        Err(err) => Err(err),
                    }
                }
                "eth_unsubscribe" => {
                    // TODO: move this logic into the app?
                    let request_bytes = json_request.num_bytes();

                    let request_metadata =
                        Arc::new(RequestMetadata::new(REQUEST_PERIOD, request_bytes).unwrap());

                    let subscription_id = json_request.params.unwrap().to_string();

                    // TODO: is this the right response?
                    let partial_response = match subscriptions.remove(&subscription_id) {
                        None => false,
                        Some(handle) => {
                            handle.abort();
                            true
                        }
                    };

                    let response =
                        JsonRpcForwardedResponse::from_value(json!(partial_response), id.clone());

                    if let Some(stat_sender) = app.stat_sender.as_ref() {
                        let response_stat = ProxyResponseStat::new(
                            json_request.method.clone(),
                            authorization.clone(),
                            request_metadata,
                            response.num_bytes(),
                        );

                        if let Err(err) = stat_sender.send_async(response_stat.into()).await {
                            // TODO: what should we do?
                            warn!("stat_sender failed during eth_unsubscribe: {:?}", err);
                        }
                    }

                    Ok(response.into())
                }
                _ => {
                    let (response, _) = app
                        .proxy_web3_rpc(authorization.clone(), json_request.into())
                        .await
                        // TODO: DO NOT UNWRAP HERE! ANY FAILING MESSAGES WILL KEPP THE CONNECTION!
                        .unwrap();

                    Ok(response)
                }
            };

            (id, response)
        }
        Err(err) => {
            // TODO: move this logic somewhere else and just set id to None here
            let id = RawValue::from_string("null".to_string()).expect("null can always be a value");
            (id, Err(err.into()))
        }
    };

    let response_str = match response {
        Ok(x) => serde_json::to_string(&x).expect("to_string should always work here"),
        Err(err) => {
            // we have an anyhow error. turn it into a response
            let response = JsonRpcForwardedResponse::from_anyhow_error(err, None, Some(id));

            serde_json::to_string(&response).expect("to_string should always work here")
        }
    };

    // TODO: what error should this be?

    Message::Text(response_str)
}

async fn read_web3_socket(
    app: Arc<Web3ProxyApp>,
    authorization: Arc<Authorization>,
    mut ws_rx: SplitStream<WebSocket>,
    response_sender: flume::Sender<Message>,
) {
    let mut subscriptions = HashMap::new();
    let subscription_count = AtomicUsize::new(1);

    while let Some(Ok(msg)) = ws_rx.next().await {
        // TODO: spawn this?
        // new message from our client. forward to a backend and then send it through response_tx
        let response_msg = match msg {
            Message::Text(payload) => {
                handle_socket_payload(
                    app.clone(),
                    &authorization,
                    &payload,
                    &response_sender,
                    &subscription_count,
                    &mut subscriptions,
                )
                .await
            }
            Message::Ping(x) => {
                trace!("ping: {:?}", x);
                Message::Pong(x)
            }
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
                    &authorization,
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

    // TODO: is there any way to make this stream receive.
    while let Ok(msg) = response_rx.recv_async().await {
        // a response is ready

        // TODO: poke rate limits for this user?

        // forward the response to through the websocket
        if let Err(err) = ws_tx.send(msg).await {
            // this is common. it happens whenever a client disconnects
            trace!("unable to write to websocket: {:?}", err);
            break;
        };
    }

    // TODO: decrement counter for open websockets
}
