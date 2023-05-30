//! Take a user's WebSocket JSON-RPC requests and either respond from local data or proxy the request to a backend rpc server.
//!
//! WebSockets are the preferred method of receiving requests, but not all clients have good support.

use super::authorization::{ip_is_authorized, key_is_authorized, Authorization, RequestMetadata};
use super::errors::{Web3ProxyError, Web3ProxyResponse};
use crate::jsonrpc::JsonRpcId;
use crate::{
    app::Web3ProxyApp,
    frontend::errors::Web3ProxyResult,
    jsonrpc::{JsonRpcForwardedResponse, JsonRpcForwardedResponseEnum, JsonRpcRequest},
};
use anyhow::Context;
use axum::headers::{Origin, Referer, UserAgent};
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::Path,
    response::{IntoResponse, Redirect},
    Extension, TypedHeader,
};
use axum_client_ip::InsecureClientIp;
use axum_macros::debug_handler;
use ethers::types::U64;
use fstrings::{f, format_args_f};
use futures::SinkExt;
use futures::{
    future::AbortHandle,
    stream::{SplitSink, SplitStream, StreamExt},
};
use handlebars::Handlebars;
use hashbrown::HashMap;
use http::StatusCode;
use log::{info, trace};
use serde_json::json;
use std::sync::Arc;
use std::{str::from_utf8_mut, sync::atomic::AtomicUsize};
use tokio::sync::{broadcast, OwnedSemaphorePermit, RwLock};

/// How to select backend servers for a request
#[derive(Copy, Clone, Debug)]
pub enum ProxyMode {
    /// send to the "best" synced server
    Best,
    /// send to all synced servers and return the fastest non-error response (reverts do not count as errors here)
    Fastest(usize),
    /// send to all servers for benchmarking. return the fastest non-error response
    Versus,
    /// send all requests and responses to kafka
    /// TODO: should this be seperate from best/fastest/versus?
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
) -> Web3ProxyResponse {
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
) -> Web3ProxyResponse {
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
) -> Web3ProxyResponse {
    // TODO: config to disable this
    _websocket_handler(ProxyMode::Versus, app, ip, origin, ws_upgrade).await
}

async fn _websocket_handler(
    proxy_mode: ProxyMode,
    app: Arc<Web3ProxyApp>,
    InsecureClientIp(ip): InsecureClientIp,
    origin: Option<TypedHeader<Origin>>,
    ws_upgrade: Option<WebSocketUpgrade>,
) -> Web3ProxyResponse {
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
                Err(Web3ProxyError::WebsocketOnly)
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
) -> Web3ProxyResponse {
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
) -> Web3ProxyResponse {
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
) -> Web3ProxyResponse {
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
) -> Web3ProxyResponse {
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
) -> Web3ProxyResponse {
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
                (None, None, _) => Err(Web3ProxyError::StatusCode(
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
                        Err(Web3ProxyError::StatusCode(
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
                _ => Err(Web3ProxyError::StatusCode(
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
/// TODO: i think this subscriptions hashmap grows unbounded
async fn handle_socket_payload(
    app: Arc<Web3ProxyApp>,
    authorization: &Arc<Authorization>,
    payload: &str,
    response_sender: &flume::Sender<Message>,
    subscription_count: &AtomicUsize,
    subscriptions: Arc<RwLock<HashMap<U64, AbortHandle>>>,
) -> Web3ProxyResult<(Message, Option<OwnedSemaphorePermit>)> {
    let (authorization, semaphore) = match authorization.check_again(&app).await {
        Ok((a, s)) => (a, s),
        Err(err) => {
            let (_, err) = err.into_response_parts();

            let err = JsonRpcForwardedResponse::from_response_data(err, Default::default());

            let err = serde_json::to_string(&err)?;

            return Ok((Message::Text(err), None));
        }
    };

    // TODO: do any clients send batches over websockets?
    // TODO: change response into response_data
    let (response_id, response) = match serde_json::from_str::<JsonRpcRequest>(payload) {
        Ok(json_request) => {
            let response_id = json_request.id.clone();

            // TODO: move this to a seperate function so we can use the try operator
            let response: Web3ProxyResult<JsonRpcForwardedResponseEnum> =
                match &json_request.method[..] {
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
                                {
                                    let mut x = subscriptions.write().await;

                                    let result: &serde_json::value::RawValue = response
                                        .result
                                        .as_ref()
                                        .context("there should be a result here")?;

                                    // TODO: there must be a better way to turn a RawValue
                                    let k: U64 = serde_json::from_str(result.get())
                                        .context("subscription ids must be U64s")?;

                                    x.insert(k, handle);
                                };

                                Ok(response.into())
                            }
                            Err(err) => Err(err),
                        }
                    }
                    "eth_unsubscribe" => {
                        let request_metadata =
                            RequestMetadata::new(&app, authorization.clone(), &json_request, None)
                                .await;

                        #[derive(serde::Deserialize)]
                        struct EthUnsubscribeParams([U64; 1]);

                        if let Some(params) = json_request.params {
                            match serde_json::from_value(params) {
                                Ok::<EthUnsubscribeParams, _>(params) => {
                                    let subscription_id = &params.0[0];

                                    // TODO: is this the right response?
                                    let partial_response = {
                                        let mut x = subscriptions.write().await;
                                        match x.remove(subscription_id) {
                                            None => false,
                                            Some(handle) => {
                                                handle.abort();
                                                true
                                            }
                                        }
                                    };

                                    // TODO: don't create the response here. use a JsonRpcResponseData instead
                                    let response = JsonRpcForwardedResponse::from_value(
                                        json!(partial_response),
                                        response_id.clone(),
                                    );

                                    request_metadata.add_response(&response);

                                    Ok(response.into())
                                }
                                Err(err) => Err(Web3ProxyError::BadRequest(f!(
                                    "incorrect params given for eth_unsubscribe. {err:?}"
                                ))),
                            }
                        } else {
                            Err(Web3ProxyError::BadRequest(
                                "no params given for eth_unsubscribe".to_string(),
                            ))
                        }
                    }
                    _ => app
                        .proxy_web3_rpc(authorization.clone(), json_request.into())
                        .await
                        .map(|(_status_code, response, _)| response),
                };

            (response_id, response)
        }
        Err(err) => {
            let id = JsonRpcId::None.to_raw_value();
            (id, Err(err.into()))
        }
    };

    let response_str = match response {
        Ok(x) => serde_json::to_string(&x).expect("to_string should always work here"),
        Err(err) => {
            let (_, response_data) = err.into_response_parts();

            let response = JsonRpcForwardedResponse::from_response_data(response_data, response_id);

            serde_json::to_string(&response).expect("to_string should always work here")
        }
    };

    Ok((Message::Text(response_str), semaphore))
}

async fn read_web3_socket(
    app: Arc<Web3ProxyApp>,
    authorization: Arc<Authorization>,
    mut ws_rx: SplitStream<WebSocket>,
    response_sender: flume::Sender<Message>,
) {
    // RwLock should be fine here. a user isn't going to be opening tons of subscriptions
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
                            Message::Text(ref payload) => {
                                // TODO: do not unwrap!
                                let (msg, s) = handle_socket_payload(
                                    app.clone(),
                                    &authorization,
                                    payload,
                                    &response_sender,
                                    &subscription_count,
                                    subscriptions,
                                )
                                .await.unwrap();

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

                                // TODO: do not unwrap!
                                let (msg, s) = handle_socket_payload(
                                    app.clone(),
                                    &authorization,
                                    payload,
                                    &response_sender,
                                    &subscription_count,
                                    subscriptions,
                                )
                                .await.unwrap();

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
