//! Websocket-specific functions for the Web3ProxyApp

use super::App;
use crate::errors::{Web3ProxyError, Web3ProxyResult};
use crate::frontend::authorization::RequestOrMethod;
use crate::jsonrpc::{self, ValidatedRequest};
use crate::response_cache::ForwardedResponse;
use axum::extract::ws::{CloseFrame, Message};
use deferred_rate_limiter::DeferredRateLimitResult;
use ethers::types::U64;
use futures::future::AbortHandle;
use futures::future::Abortable;
use futures::stream::StreamExt;
use http::StatusCode;
use serde_json::json;
use std::sync::atomic::{self, AtomicU64};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::WatchStream;
use tracing::{error, trace};

impl App {
    pub async fn eth_subscribe<'a>(
        self: &'a Arc<Self>,
        web3_request: Arc<ValidatedRequest>,
        subscription_count: &'a AtomicU64,
        // TODO: taking a sender for Message instead of the exact json we are planning to send feels wrong, but its easier for now
        response_sender: mpsc::Sender<Message>,
    ) -> Web3ProxyResult<(AbortHandle, jsonrpc::ParsedResponse)> {
        let subscribe_to = web3_request
            .inner
            .params()
            .get(0)
            .and_then(|x| x.as_str())
            .ok_or_else(|| {
                Web3ProxyError::BadRequest("unable to subscribe using these params".into())
            })?;

        // anyone can subscribe to newHeads
        // only premium users are allowed to subscribe to the other things
        if !(self.config.free_subscriptions
            || subscribe_to == "newHeads"
            || web3_request.authorization.active_premium().await)
        {
            return Err(Web3ProxyError::AccessDenied(
                "eth_subscribe for this event requires an active premium account".into(),
            ));
        }

        let (subscription_abort_handle, subscription_registration) = AbortHandle::new_pair();

        // TODO: this only needs to be unique per connection. we don't need it globably unique
        // TODO: have a max number of subscriptions per key/ip. have a global max number of subscriptions? how should this be calculated?
        let subscription_id = subscription_count.fetch_add(1, atomic::Ordering::SeqCst);
        let subscription_id = U64::from(subscription_id);

        // TODO: calling `json!` on every request is probably not fast. but it works for now
        // TODO: i think we need a stricter EthSubscribeRequest type that JsonRpcRequest can turn into
        // TODO: DRY This up. lots of duplication between newHeads and newPendingTransactions
        match subscribe_to {
            "newHeads" => {
                // we clone the watch before spawning so that theres less chance of missing anything
                // TODO: watch receivers can miss a block. is that okay?
                let head_block_receiver = self.watch_consensus_head_receiver.clone();
                let app = self.clone();
                let authorization = web3_request.authorization.clone();

                tokio::spawn(async move {
                    trace!("newHeads subscription {:?}", subscription_id);

                    let mut head_block_receiver = Abortable::new(
                        WatchStream::new(head_block_receiver),
                        subscription_registration,
                    );

                    while let Some(new_head) = head_block_receiver.next().await {
                        let new_head = if let Some(new_head) = new_head {
                            new_head
                        } else {
                            continue;
                        };

                        // todo!(this needs a permit)
                        let subscription_web3_request = ValidatedRequest::new_with_app(
                            &app,
                            authorization.clone(),
                            None,
                            None,
                            RequestOrMethod::Method("eth_subscribe(newHeads)".into(), 0),
                            Some(new_head),
                            None,
                        )
                        .await;

                        match subscription_web3_request {
                            Err(err) => {
                                error!(?err, "error creating subscription_web3_request");
                                // TODO: send them an error message before closing
                                break;
                            }
                            Ok(subscription_web3_request) => {
                                if let Some(close_message) = app
                                    .rate_limit_close_websocket(&subscription_web3_request)
                                    .await
                                {
                                    // TODO: send them a message so they know they were rate limited
                                    let _ = response_sender.send(close_message).await;
                                    break;
                                }

                                // TODO: make a struct for this? using our SingleForwardedResponse won't work because it needs an id
                                let response_json = json!({
                                    "jsonrpc": "2.0",
                                    "method":"eth_subscription",
                                    "params": {
                                        "subscription": subscription_id,
                                        // TODO: option to include full transaction objects instead of just the hashes?
                                        "result": subscription_web3_request.head_block.as_ref().map(|x| &x.0),
                                    },
                                });

                                let response_str = serde_json::to_string(&response_json)
                                    .expect("this should always be valid json");

                                // we could use ForwardedResponse::num_bytes() here, but since we already have the string, this is easier
                                let response_bytes = response_str.len() as u64;

                                // TODO: do clients support binary messages?
                                // TODO: can we check a content type header?
                                let response_msg = Message::Text(response_str);

                                if response_sender.send(response_msg).await.is_err() {
                                    // TODO: increment error_response? i don't think so. i think this will happen once every time a client disconnects.
                                    // TODO: cancel this subscription earlier? select on head_block_receiver.next() and an abort handle?
                                    break;
                                };

                                subscription_web3_request.add_response(response_bytes);
                            }
                        }
                    }

                    let _ = response_sender.send(Message::Close(None)).await;

                    trace!("closed newHeads subscription {:?}", subscription_id);
                });
            }
            // TODO: bring back the other custom subscription types that had the full transaction object
            "newPendingTransactions" => {
                // we subscribe before spawning so that theres less chance of missing anything
                let pending_txid_firehose = self.pending_txid_firehose.subscribe();
                let app = self.clone();
                let authorization = web3_request.authorization.clone();

                tokio::spawn(async move {
                    let mut pending_txid_firehose = Abortable::new(
                        BroadcastStream::new(pending_txid_firehose),
                        subscription_registration,
                    );

                    while let Some(maybe_txid) = pending_txid_firehose.next().await {
                        match maybe_txid {
                            Err(err) => {
                                trace!(
                                    ?err,
                                    "error inside newPendingTransactions. probably lagged"
                                );
                                continue;
                            }
                            Ok(new_txid) => {
                                // TODO: include the head_block here?
                                // todo!(this needs a permit)
                                match ValidatedRequest::new_with_app(
                                    &app,
                                    authorization.clone(),
                                    None,
                                    None,
                                    RequestOrMethod::Method(
                                        "eth_subscribe(newPendingTransactions)".into(),
                                        0,
                                    ),
                                    None,
                                    None,
                                )
                                .await
                                {
                                    Err(err) => {
                                        error!(?err, "error creating subscription_web3_request");
                                        // what should we do to turn this error into a message for them?
                                        break;
                                    }
                                    Ok(subscription_web3_request) => {
                                        // check if we should close the websocket connection
                                        if let Some(close_message) = app
                                            .rate_limit_close_websocket(&subscription_web3_request)
                                            .await
                                        {
                                            let _ = response_sender.send(close_message).await;
                                            break;
                                        }

                                        // TODO: make a struct/helper function for this
                                        let response_json = json!({
                                            "jsonrpc": "2.0",
                                            "method":"eth_subscription",
                                            "params": {
                                                "subscription": subscription_id,
                                                "result": new_txid,
                                            },
                                        });

                                        let response_str = serde_json::to_string(&response_json)
                                            .expect("this should always be valid json");

                                        // we could use ForwardedResponse::num_bytes() here, but since we already have the string, this is easier
                                        let response_bytes = response_str.len() as u64;

                                        subscription_web3_request.add_response(response_bytes);

                                        // TODO: do clients support binary messages?
                                        // TODO: can we check a content type header?
                                        let response_msg = Message::Text(response_str);

                                        if response_sender.send(response_msg).await.is_err() {
                                            // TODO: increment error_response? i don't think so. i think this will happen once every time a client disconnects.
                                            // TODO: cancel this subscription earlier? select on head_block_receiver.next() and an abort handle?
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }

                    let _ = response_sender.send(Message::Close(None)).await;

                    trace!(
                        "closed newPendingTransactions subscription {:?}",
                        subscription_id
                    );
                });
            }
            _ => {
                // TODO: make sure this gets a CU cost of unimplemented instead of the normal eth_subscribe cost?
                return Err(Web3ProxyError::MethodNotFound(
                    subscribe_to.to_owned().into(),
                ));
            }
        };

        // TODO: do something with subscription_join_handle?

        let response_data = ForwardedResponse::from(json!(subscription_id));

        let response =
            jsonrpc::ParsedResponse::from_response_data(response_data, web3_request.id());

        // TODO: better way of passing in ParsedResponse
        let response = jsonrpc::SingleResponse::Parsed(response);
        // TODO: this serializes twice
        web3_request.add_response(&response);
        let response = response.parsed().await.expect("Response already parsed");

        // TODO: make a `SubscriptonHandle(AbortHandle, JoinHandle)` struct?
        Ok((subscription_abort_handle, response))
    }

    async fn rate_limit_close_websocket(&self, web3_request: &ValidatedRequest) -> Option<Message> {
        let authorization = &web3_request.authorization;

        if !authorization.active_premium().await {
            if let Some(rate_limiter) = &self.frontend_public_rate_limiter {
                match rate_limiter
                    .throttle(
                        authorization.ip,
                        authorization.checks.max_requests_per_period,
                        1,
                    )
                    .await
                {
                    Ok(DeferredRateLimitResult::RetryNever) => {
                        let close_frame = CloseFrame {
                            code: StatusCode::TOO_MANY_REQUESTS.as_u16(),
                            reason:
                                "rate limited. upgrade to premium for unlimited websocket messages"
                                    .into(),
                        };

                        return Some(Message::Close(Some(close_frame)));
                    }
                    Ok(DeferredRateLimitResult::RetryAt(retry_at)) => {
                        let retry_at = retry_at.duration_since(Instant::now());

                        let reason = format!("rate limited. upgrade to premium for unlimited websocket messages. retry in {}s", retry_at.as_secs_f32());

                        let close_frame = CloseFrame {
                            code: StatusCode::TOO_MANY_REQUESTS.as_u16(),
                            reason: reason.into(),
                        };

                        return Some(Message::Close(Some(close_frame)));
                    }
                    Ok(_) => {}
                    Err(err) => {
                        // this an internal error of some kind, not the rate limit being hit
                        error!(?err, "rate limiter is unhappy. allowing ip");
                    }
                }
            }
        }

        None
    }
}
