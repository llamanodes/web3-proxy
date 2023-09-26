//! Websocket-specific functions for the Web3ProxyApp

use super::Web3ProxyApp;
use crate::errors::{Web3ProxyError, Web3ProxyResult};
use crate::frontend::authorization::{Authorization, RequestMetadata, RequestOrMethod};
use crate::jsonrpc::{self, JsonRpcRequest};
use crate::response_cache::JsonRpcResponseEnum;
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

impl Web3ProxyApp {
    pub async fn eth_subscribe<'a>(
        self: &'a Arc<Self>,
        authorization: Arc<Authorization>,
        jsonrpc_request: JsonRpcRequest,
        subscription_count: &'a AtomicU64,
        // TODO: taking a sender for Message instead of the exact json we are planning to send feels wrong, but its easier for now
        response_sender: mpsc::Sender<Message>,
    ) -> Web3ProxyResult<(AbortHandle, jsonrpc::ParsedResponse)> {
        let subscribe_to = jsonrpc_request
            .params
            .get(0)
            .and_then(|x| x.as_str())
            .ok_or_else(|| {
                Web3ProxyError::BadRequest("unable to subscribe using these params".into())
            })?;

        // anyone can subscribe to newHeads
        // only premium users are allowed to subscribe to the other things
        if !(subscribe_to == "newHeads" || authorization.active_premium().await) {
            return Err(Web3ProxyError::AccessDenied(
                "eth_subscribe for this event requires an active premium account".into(),
            ));
        }

        let request_metadata = RequestMetadata::new(
            self,
            authorization.clone(),
            RequestOrMethod::Request(&jsonrpc_request),
            None,
        )
        .await;

        let (subscription_abort_handle, subscription_registration) = AbortHandle::new_pair();

        // TODO: this only needs to be unique per connection. we don't need it globably unique
        // TODO: have a max number of subscriptions per key/ip. have a global max number of subscriptions? how should this be calculated?
        let subscription_id = subscription_count.fetch_add(1, atomic::Ordering::SeqCst);
        let subscription_id = U64::from(subscription_id);

        // save the id so we can use it in the response
        let id = jsonrpc_request.id.clone();

        // TODO: calling `json!` on every request is probably not fast. but it works for now
        // TODO: i think we need a stricter EthSubscribeRequest type that JsonRpcRequest can turn into
        // TODO: DRY This up. lots of duplication between newHeads and newPendingTransactions
        match subscribe_to {
            "newHeads" => {
                let head_block_receiver = self.watch_consensus_head_receiver.clone();
                let app = self.clone();

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

                        let subscription_request_metadata = RequestMetadata::new(
                            &app,
                            authorization.clone(),
                            RequestOrMethod::Method("eth_subscribe(newHeads)", 0),
                            Some(&new_head),
                        )
                        .await;

                        if let Some(close_message) = app
                            .rate_limit_close_websocket(&subscription_request_metadata)
                            .await
                        {
                            let _ = response_sender.send(close_message).await;
                            break;
                        }

                        // TODO: make a struct for this? using our JsonRpcForwardedResponse won't work because it needs an id
                        let response_json = json!({
                            "jsonrpc": "2.0",
                            "method":"eth_subscription",
                            "params": {
                                "subscription": subscription_id,
                                // TODO: option to include full transaction objects instead of just the hashes?
                                "result": new_head.block,
                            },
                        });

                        let response_str = serde_json::to_string(&response_json)
                            .expect("this should always be valid json");

                        // we could use JsonRpcForwardedResponseEnum::num_bytes() here, but since we already have the string, this is easier
                        let response_bytes = response_str.len();

                        // TODO: do clients support binary messages?
                        // TODO: can we check a content type header?
                        let response_msg = Message::Text(response_str);

                        if response_sender.send(response_msg).await.is_err() {
                            // TODO: increment error_response? i don't think so. i think this will happen once every time a client disconnects.
                            // TODO: cancel this subscription earlier? select on head_block_receiver.next() and an abort handle?
                            break;
                        };

                        subscription_request_metadata.add_response(response_bytes);
                    }

                    trace!("closed newHeads subscription {:?}", subscription_id);
                });
            }
            // TODO: bring back the other custom subscription types that had the full transaction object
            "newPendingTransactions" => {
                let pending_txid_firehose = self.pending_txid_firehose.subscribe();
                let app = self.clone();

                tokio::spawn(async move {
                    let mut pending_txid_firehose = Abortable::new(
                        BroadcastStream::new(pending_txid_firehose),
                        subscription_registration,
                    );

                    while let Some(Ok(new_txid)) = pending_txid_firehose.next().await {
                        // TODO: include the head_block here?
                        let subscription_request_metadata = RequestMetadata::new(
                            &app,
                            authorization.clone(),
                            RequestOrMethod::Method("eth_subscribe(newPendingTransactions)", 0),
                            None,
                        )
                        .await;

                        // check if we should close the websocket connection
                        if let Some(close_message) = app
                            .rate_limit_close_websocket(&subscription_request_metadata)
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

                        // we could use JsonRpcForwardedResponseEnum::num_bytes() here, but since we already have the string, this is easier
                        let response_bytes = response_str.len();

                        subscription_request_metadata.add_response(response_bytes);

                        // TODO: do clients support binary messages?
                        // TODO: can we check a content type header?
                        let response_msg = Message::Text(response_str);

                        if response_sender.send(response_msg).await.is_err() {
                            // TODO: increment error_response? i don't think so. i think this will happen once every time a client disconnects.
                            // TODO: cancel this subscription earlier? select on head_block_receiver.next() and an abort handle?
                            break;
                        };
                    }

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

        let response_data = JsonRpcResponseEnum::from(json!(subscription_id));

        let response = jsonrpc::ParsedResponse::from_response_data(response_data, id);

        // TODO: better way of passing in ParsedResponse
        let response = jsonrpc::SingleResponse::Parsed(response);
        // TODO: this serializes twice
        request_metadata.add_response(&response);
        let response = response.parsed().await.expect("Response already parsed");

        // TODO: make a `SubscriptonHandle(AbortHandle, JoinHandle)` struct?
        Ok((subscription_abort_handle, response))
    }

    async fn rate_limit_close_websocket(
        &self,
        request_metadata: &RequestMetadata,
    ) -> Option<Message> {
        if let Some(authorization) = request_metadata.authorization.as_ref() {
            if authorization.checks.rpc_secret_key_id.is_none() {
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
                            // TODO: i really want axum to do this for us in a single place.
                            error!(?err, "rate limiter is unhappy. allowing ip");
                        }
                    }
                }
            }
        }

        None
    }
}
