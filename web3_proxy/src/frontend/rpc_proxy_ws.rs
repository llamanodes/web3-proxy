//! Take a user's WebSocket JSON-RPC requests and either respond from local data or proxy the request to a backend rpc server.
//!
//! WebSockets are the preferred method of receiving requests, but not all clients have good support.

use super::authorization::{ip_is_authorized, key_is_authorized, Authorization, RequestMetadata};
use super::errors::{FrontendErrorResponse, FrontendResult};
use crate::stats::RpcQueryStats;
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
use axum_client_ip::InsecureClientIp;
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
use serde_json::json;
use serde_json::value::to_raw_value;
use std::sync::Arc;
use std::{str::from_utf8_mut, sync::atomic::AtomicUsize};
use tokio::sync::{broadcast, OwnedSemaphorePermit, RwLock};

#[derive(Copy, Clone, Debug)]
pub enum ProxyMode {
    /// send to the "best" synced server
    Best,
    /// send to all synced servers and return the fastest non-error response (reverts do not count as errors here)
    Fastest(usize),
    /// send to all servers for benchmarking. return the fastest non-error response
    Versus,
    /// send all requests and responses to kafka
    Debug,
}

impl Default for ProxyMode {
    fn default() -> Self {
        Self::Best
    }
}

/// Public entrypoint for WebSocket JSON-RPC requests.
/// Queries a single server at a time
#[debug_handler]
pub async fn websocket_handler(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ip: InsecureClientIp,
    origin: Option<TypedHeader<Origin>>,
    ws_upgrade: Option<WebSocketUpgrade>,
) -> FrontendResult {
    _websocket_handler(ProxyMode::Best, app, ip, origin, ws_upgrade).await
}

/// Public entrypoint for WebSocket JSON-RPC requests that uses all synced servers.
/// Queries all synced backends with every request! This might get expensive!
#[debug_handler]
pub async fn fastest_websocket_handler(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ip: InsecureClientIp,
    origin: Option<TypedHeader<Origin>>,
    ws_upgrade: Option<WebSocketUpgrade>,
) -> FrontendResult {
    // TODO: get the fastest number from the url params (default to 0/all)
    // TODO: config to disable this
    _websocket_handler(ProxyMode::Fastest(0), app, ip, origin, ws_upgrade).await
}

/// Public entrypoint for WebSocket JSON-RPC requests that uses all synced servers.
/// Queries **all** backends with every request! This might get expensive!
#[debug_handler]
pub async fn versus_websocket_handler(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ip: InsecureClientIp,
    origin: Option<TypedHeader<Origin>>,
    ws_upgrade: Option<WebSocketUpgrade>,
) -> FrontendResult {
    // TODO: config to disable this
    _websocket_handler(ProxyMode::Versus, app, ip, origin, ws_upgrade).await
}

async fn _websocket_handler(
    proxy_mode: ProxyMode,
    app: Arc<Web3ProxyApp>,
    InsecureClientIp(ip): InsecureClientIp,
    origin: Option<TypedHeader<Origin>>,
    ws_upgrade: Option<WebSocketUpgrade>,
) -> FrontendResult {
    let origin = origin.map(|x| x.0);

    let (authorization, _semaphore) = ip_is_authorized(&app, ip, origin, proxy_mode).await?;

    let authorization = Arc::new(authorization);

    match ws_upgrade {
        Some(ws) => Ok(ws
            .on_upgrade(move |socket| proxy_web3_socket(app, authorization, socket))
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
    ip: InsecureClientIp,
    Path(rpc_key): Path<String>,
    origin: Option<TypedHeader<Origin>>,
    referer: Option<TypedHeader<Referer>>,
    user_agent: Option<TypedHeader<UserAgent>>,
    ws_upgrade: Option<WebSocketUpgrade>,
) -> FrontendResult {
    _websocket_handler_with_key(
        ProxyMode::Best,
        app,
        ip,
        rpc_key,
        origin,
        referer,
        user_agent,
        ws_upgrade,
    )
    .await
}

#[debug_handler]
pub async fn debug_websocket_handler_with_key(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ip: InsecureClientIp,
    Path(rpc_key): Path<String>,
    origin: Option<TypedHeader<Origin>>,
    referer: Option<TypedHeader<Referer>>,
    user_agent: Option<TypedHeader<UserAgent>>,
    ws_upgrade: Option<WebSocketUpgrade>,
) -> FrontendResult {
    _websocket_handler_with_key(
        ProxyMode::Debug,
        app,
        ip,
        rpc_key,
        origin,
        referer,
        user_agent,
        ws_upgrade,
    )
    .await
}

#[debug_handler]
pub async fn fastest_websocket_handler_with_key(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ip: InsecureClientIp,
    Path(rpc_key): Path<String>,
    origin: Option<TypedHeader<Origin>>,
    referer: Option<TypedHeader<Referer>>,
    user_agent: Option<TypedHeader<UserAgent>>,
    ws_upgrade: Option<WebSocketUpgrade>,
) -> FrontendResult {
    // TODO: get the fastest number from the url params (default to 0/all)
    _websocket_handler_with_key(
        ProxyMode::Fastest(0),
        app,
        ip,
        rpc_key,
        origin,
        referer,
        user_agent,
        ws_upgrade,
    )
    .await
}

#[debug_handler]
pub async fn versus_websocket_handler_with_key(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ip: InsecureClientIp,
    Path(rpc_key): Path<String>,
    origin: Option<TypedHeader<Origin>>,
    referer: Option<TypedHeader<Referer>>,
    user_agent: Option<TypedHeader<UserAgent>>,
    ws_upgrade: Option<WebSocketUpgrade>,
) -> FrontendResult {
    _websocket_handler_with_key(
        ProxyMode::Versus,
        app,
        ip,
        rpc_key,
        origin,
        referer,
        user_agent,
        ws_upgrade,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn _websocket_handler_with_key(
    proxy_mode: ProxyMode,
    app: Arc<Web3ProxyApp>,
    InsecureClientIp(ip): InsecureClientIp,
    rpc_key: String,
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
        proxy_mode,
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
                authorization.checks.rpc_secret_key_id,
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

                    if authorization.checks.rpc_secret_key_id.is_none() {
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
    subscriptions: Arc<RwLock<HashMap<String, AbortHandle>>>,
) -> (Message, Option<OwnedSemaphorePermit>) {
    let (authorization, semaphore) = match authorization.check_again(&app).await {
        Ok((a, s)) => (a, s),
        Err(err) => {
            let (_, err) = err.into_response_parts();

            let err = serde_json::to_string(&err).expect("to_string should always work here");

            return (Message::Text(err), None);
        }
    };

    // TODO: do any clients send batches over websockets?
    let (id, response) = match serde_json::from_str::<JsonRpcRequest>(payload) {
        Ok(json_request) => {
            let id = json_request.id.clone();

            let response: anyhow::Result<JsonRpcForwardedResponseEnum> = match &json_request.method
                [..]
            {
                "eth_subscribe" => {
                    // TODO: how can we subscribe with proxy_mode?
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
                            let mut x = subscriptions.write().await;

                            x.insert(
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

                    let request_metadata = Arc::new(RequestMetadata::new(request_bytes).unwrap());

                    let subscription_id = json_request.params.unwrap().to_string();

                    let mut x = subscriptions.write().await;

                    // TODO: is this the right response?
                    let partial_response = match x.remove(&subscription_id) {
                        None => false,
                        Some(handle) => {
                            handle.abort();
                            true
                        }
                    };

                    drop(x);

                    let response =
                        JsonRpcForwardedResponse::from_value(json!(partial_response), id.clone());

                    if let Some(stat_sender) = app.stat_sender.as_ref() {
                        let response_stat = RpcQueryStats::new(
                            Some(json_request.method.clone()),
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
                _ => app
                    .proxy_web3_rpc(authorization.clone(), json_request.into())
                    .await
                    .map_or_else(
                        |err| match err {
                            FrontendErrorResponse::Anyhow(err) => Err(err),
                            _ => {
                                error!("handle this better! {:?}", err);
                                Err(anyhow::anyhow!("unexpected error! {:?}", err))
                            }
                        },
                        |(response, _)| Ok(response),
                    ),
            };

            (id, response)
        }
        Err(err) => {
            // TODO: move this logic somewhere else and just set id to None here
            let id =
                to_raw_value(&json!(None::<Option::<()>>)).expect("None can always be a RawValue");
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

    (Message::Text(response_str), semaphore)
}

async fn read_web3_socket(
    app: Arc<Web3ProxyApp>,
    authorization: Arc<Authorization>,
    mut ws_rx: SplitStream<WebSocket>,
    response_sender: flume::Sender<Message>,
) {
    // TODO: need a concurrent hashmap
    let subscriptions = Arc::new(RwLock::new(HashMap::new()));
    let subscription_count = Arc::new(AtomicUsize::new(1));

    let (close_sender, mut close_receiver) = broadcast::channel(1);

    loop {
        tokio::select! {
            msg = ws_rx.next() => {
                if let Some(Ok(msg)) = msg {
                    // spawn so that we can serve responses from this loop even faster
                    // TODO: only do these clones if the msg is text/binary?
                    let close_sender = close_sender.clone();
                    let app = app.clone();
                    let authorization = authorization.clone();
                    let response_sender = response_sender.clone();
                    let subscriptions = subscriptions.clone();
                    let subscription_count = subscription_count.clone();

                    let f = async move {
                        let mut _semaphore = None;

                        // new message from our client. forward to a backend and then send it through response_tx
                        let response_msg = match msg {
                            Message::Text(payload) => {
                                let (msg, s) = handle_socket_payload(
                                    app.clone(),
                                    &authorization,
                                    &payload,
                                    &response_sender,
                                    &subscription_count,
                                    subscriptions,
                                )
                                .await;

                                _semaphore = s;

                                msg
                            }
                            Message::Ping(x) => {
                                trace!("ping: {:?}", x);
                                Message::Pong(x)
                            }
                            Message::Pong(x) => {
                                trace!("pong: {:?}", x);
                                return;
                            }
                            Message::Close(_) => {
                                info!("closing websocket connection");
                                // TODO: do something to close subscriptions?
                                let _ = close_sender.send(true);
                                return;
                            }
                            Message::Binary(mut payload) => {
                                let payload = from_utf8_mut(&mut payload).unwrap();

                                let (msg, s) = handle_socket_payload(
                                    app.clone(),
                                    &authorization,
                                    payload,
                                    &response_sender,
                                    &subscription_count,
                                    subscriptions,
                                )
                                .await;

                                _semaphore = s;

                                msg
                            }
                        };

                        if response_sender.send_async(response_msg).await.is_err() {
                            let _ = close_sender.send(true);
                            return;
                        };

                        _semaphore = None;
                    };

                    tokio::spawn(f);
                } else {
                    break;
                }
            }
            _ = close_receiver.recv() => {
                break;
            }
        }
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
