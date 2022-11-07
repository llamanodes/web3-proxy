//! Take a user's WebSocket JSON-RPC requests and either respond from local data or proxy the request to a backend rpc server.
//!
//! WebSockets are the preferred method of receiving requests, but not all clients have good support.

use super::authorization::{ip_is_authorized, key_is_authorized, AuthorizedRequest};
use super::errors::FrontendResult;
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
use serde_json::{json, value::RawValue};
use std::sync::Arc;
use std::{str::from_utf8_mut, sync::atomic::AtomicUsize};
use tracing::{error, error_span, info, instrument, trace, Instrument};

use crate::{
    app::Web3ProxyApp,
    jsonrpc::{JsonRpcForwardedResponse, JsonRpcForwardedResponseEnum, JsonRpcRequest},
};

/// Public entrypoint for WebSocket JSON-RPC requests.
/// Defaults to rate limiting by IP address, but can also read the Authorization header for a bearer token.
#[debug_handler]
#[instrument(level = "trace")]
pub async fn websocket_handler(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
    origin: Option<TypedHeader<Origin>>,
    ws_upgrade: Option<WebSocketUpgrade>,
) -> FrontendResult {
    // TODO: i don't like logging ips. move this to trace level?
    let request_span = error_span!("request", %ip, ?origin);

    let (authorized_request, _semaphore) = ip_is_authorized(&app, ip, origin)
        .instrument(request_span)
        .await?;

    let request_span = error_span!("request", ?authorized_request);

    let authorized_request = Arc::new(authorized_request);

    match ws_upgrade {
        Some(ws) => Ok(ws
            .on_upgrade(|socket| {
                proxy_web3_socket(app, authorized_request, socket).instrument(request_span)
            })
            .into_response()),
        None => {
            if let Some(redirect) = &app.config.redirect_public_url {
                // this is not a websocket. redirect to a friendly page
                Ok(Redirect::to(redirect).into_response())
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
#[instrument(level = "trace")]
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

    let request_span = error_span!("request", %ip, ?referer, ?user_agent);

    let (authorized_request, _semaphore) = key_is_authorized(
        &app,
        rpc_key,
        ip,
        origin.map(|x| x.0),
        referer.map(|x| x.0),
        user_agent.map(|x| x.0),
    )
    .instrument(request_span.clone())
    .await?;

    // TODO: type that wraps Address and have it censor? would protect us from accidently logging addresses or other user info
    let request_span = error_span!("request", ?authorized_request);

    let authorized_request = Arc::new(authorized_request);

    match ws_upgrade {
        Some(ws_upgrade) => Ok(ws_upgrade.on_upgrade(move |socket| {
            proxy_web3_socket(app, authorized_request, socket).instrument(request_span)
        })),
        None => {
            // if no websocket upgrade, this is probably a user loading the url with their browser
            if let Some(redirect) = &app.config.redirect_user_url {
                // TODO: store this on the app and use register_template?
                let reg = Handlebars::new();

                // TODO: show the user's address, not their id (remember to update the checks for {{user_id}}} in app.rs)
                // TODO: query to get the user's address. expose that instead of user_id
                if let AuthorizedRequest::User(_, authorized_key) = authorized_request.as_ref() {
                    let user_url = reg
                        .render_template(
                            redirect,
                            &json!({ "rpc_key_id": authorized_key.rpc_key_id }),
                        )
                        .expect("templating should always work");

                    // this is not a websocket. redirect to a page for this user
                    Ok(Redirect::to(&user_url).into_response())
                } else {
                    // TODO: i think this is impossible
                    Err(anyhow::anyhow!("this page is for rpcs").into())
                }
            } else {
                // TODO: do not use an anyhow error. send the user a 400
                Err(anyhow::anyhow!("redirect_user_url not set. only websockets work here").into())
            }
        }
    }
}

#[instrument(level = "trace")]
async fn proxy_web3_socket(
    app: Arc<Web3ProxyApp>,
    authorized_request: Arc<AuthorizedRequest>,
    socket: WebSocket,
) {
    // split the websocket so we can read and write concurrently
    let (ws_tx, ws_rx) = socket.split();

    // create a channel for our reader and writer can communicate. todo: benchmark different channels
    let (response_sender, response_receiver) = flume::unbounded::<Message>();

    tokio::spawn(write_web3_socket(response_receiver, ws_tx));
    tokio::spawn(read_web3_socket(
        app,
        authorized_request,
        ws_rx,
        response_sender,
    ));
}

/// websockets support a few more methods than http clients
#[instrument(level = "trace")]
async fn handle_socket_payload(
    app: Arc<Web3ProxyApp>,
    authorized_request: Arc<AuthorizedRequest>,
    payload: &str,
    response_sender: &flume::Sender<Message>,
    subscription_count: &AtomicUsize,
    subscriptions: &mut HashMap<String, AbortHandle>,
) -> Message {
    // TODO: do any clients send batches over websockets?
    let (id, response) = match serde_json::from_str::<JsonRpcRequest>(payload) {
        Ok(payload) => {
            // TODO: should we use this id for the subscription id? it should be unique and means we dont need an atomic
            let id = payload.id.clone();

            let response: anyhow::Result<JsonRpcForwardedResponseEnum> = match &payload.method[..] {
                "eth_subscribe" => {
                    // TODO: what should go in this span?
                    let span = error_span!("eth_subscribe");

                    let response = app
                        .eth_subscribe(
                            authorized_request,
                            payload,
                            subscription_count,
                            response_sender.clone(),
                        )
                        .instrument(span)
                        .await;

                    match response {
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
                    // TODO: how should handle rate limits and stats on this?
                    // TODO: handle invalid params
                    let subscription_id = payload.params.unwrap().to_string();

                    let partial_response = match subscriptions.remove(&subscription_id) {
                        None => false,
                        Some(handle) => {
                            handle.abort();
                            true
                        }
                    };

                    let response =
                        JsonRpcForwardedResponse::from_value(json!(partial_response), id.clone());

                    Ok(response.into())
                }
                _ => app.proxy_web3_rpc(authorized_request, payload.into()).await,
            };

            (id, response)
        }
        Err(err) => {
            let id = RawValue::from_string("null".to_string()).expect("null can always be a value");
            (id, Err(err.into()))
        }
    };

    let response_str = match response {
        Ok(x) => serde_json::to_string(&x),
        Err(err) => {
            // we have an anyhow error. turn it into a response
            let response = JsonRpcForwardedResponse::from_anyhow_error(err, None, Some(id));
            serde_json::to_string(&response)
        }
    }
    // TODO: what error should this be?
    .unwrap();

    Message::Text(response_str)
}

#[instrument(level = "trace")]
async fn read_web3_socket(
    app: Arc<Web3ProxyApp>,
    authorized_request: Arc<AuthorizedRequest>,
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
                    authorized_request.clone(),
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
                    authorized_request.clone(),
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

#[instrument(level = "trace")]
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
