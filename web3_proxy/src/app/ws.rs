//! Websocket-specific functions for the Web3ProxyApp

use super::Web3ProxyApp;
use crate::frontend::authorization::{Authorization, RequestMetadata, RequestOrMethod};
use crate::frontend::errors::{Web3ProxyError, Web3ProxyResult};
use crate::jsonrpc::JsonRpcForwardedResponse;
use crate::jsonrpc::JsonRpcRequest;
use crate::response_cache::JsonRpcResponseData;
use crate::rpcs::transactions::TxStatus;
use axum::extract::ws::Message;
use ethers::types::U64;
use futures::future::AbortHandle;
use futures::future::Abortable;
use futures::stream::StreamExt;
use log::trace;
use serde_json::json;
use std::sync::atomic::{self, AtomicUsize};
use std::sync::Arc;
use tokio_stream::wrappers::{BroadcastStream, WatchStream};

impl Web3ProxyApp {
    pub async fn eth_subscribe<'a>(
        self: &'a Arc<Self>,
        authorization: Arc<Authorization>,
        jsonrpc_request: JsonRpcRequest,
        subscription_count: &'a AtomicUsize,
        // TODO: taking a sender for Message instead of the exact json we are planning to send feels wrong, but its easier for now
        response_sender: flume::Sender<Message>,
    ) -> Web3ProxyResult<(AbortHandle, JsonRpcForwardedResponse)> {
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
        let subscription_id = U64::from(subscription_id as u64);

        // save the id so we can use it in the response
        let id = jsonrpc_request.id.clone();

        // TODO: calling json! on every request is probably not fast. but we can only match against
        // TODO: i think we need a stricter EthSubscribeRequest type that JsonRpcRequest can turn into
        match jsonrpc_request.params.as_ref() {
            Some(x) if x == &json!(["newHeads"]) => {
                let head_block_receiver = self.watch_consensus_head_receiver.clone();
                let app = self.clone();

                trace!("newHeads subscription {:?}", subscription_id);
                tokio::spawn(async move {
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
                            Some(new_head.number()),
                        )
                        .await;

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

                        if response_sender.send_async(response_msg).await.is_err() {
                            // TODO: increment error_response? i don't think so. i think this will happen once every time a client disconnects.
                            // TODO: cancel this subscription earlier? select on head_block_receiver.next() and an abort handle?
                            break;
                        };

                        subscription_request_metadata.add_response(response_bytes);
                    }

                    trace!("closed newHeads subscription {:?}", subscription_id);
                });
            }
            Some(x) if x == &json!(["newPendingTransactions"]) => {
                let pending_tx_receiver = self.pending_tx_sender.subscribe();
                let app = self.clone();

                let mut pending_tx_receiver = Abortable::new(
                    BroadcastStream::new(pending_tx_receiver),
                    subscription_registration,
                );

                trace!(
                    "pending newPendingTransactions subscription id: {:?}",
                    subscription_id
                );

                // TODO: do something with this handle?
                tokio::spawn(async move {
                    while let Some(Ok(new_tx_state)) = pending_tx_receiver.next().await {
                        let subscription_request_metadata = RequestMetadata::new(
                            &app,
                            authorization.clone(),
                            RequestOrMethod::Method("eth_subscribe(newPendingTransactions)", 0),
                            None,
                        )
                        .await;

                        let new_tx = match new_tx_state {
                            TxStatus::Pending(tx) => tx,
                            TxStatus::Confirmed(..) => continue,
                            TxStatus::Orphaned(tx) => tx,
                        };

                        // TODO: make a struct for this? using our JsonRpcForwardedResponse won't work because it needs an id
                        let response_json = json!({
                            "jsonrpc": "2.0",
                            "method": "eth_subscription",
                            "params": {
                                "subscription": subscription_id,
                                "result": new_tx.hash,
                            },
                        });

                        let response_str = serde_json::to_string(&response_json)
                            .expect("this should always be valid json");

                        // TODO: test that this len is the same as JsonRpcForwardedResponseEnum.num_bytes()
                        let response_bytes = response_str.len();

                        subscription_request_metadata.add_response(response_bytes);

                        // TODO: do clients support binary messages?
                        let response_msg = Message::Text(response_str);

                        if response_sender.send_async(response_msg).await.is_err() {
                            // TODO: cancel this subscription earlier? select on head_block_receiver.next() and an abort handle?
                            break;
                        };
                    }

                    trace!(
                        "closed newPendingTransactions subscription: {:?}",
                        subscription_id
                    );
                });
            }
            Some(x) if x == &json!(["newPendingFullTransactions"]) => {
                // TODO: too much copy/pasta with newPendingTransactions
                let pending_tx_receiver = self.pending_tx_sender.subscribe();
                let app = self.clone();

                let mut pending_tx_receiver = Abortable::new(
                    BroadcastStream::new(pending_tx_receiver),
                    subscription_registration,
                );

                trace!(
                    "pending newPendingFullTransactions subscription: {:?}",
                    subscription_id
                );

                // TODO: do something with this handle?
                tokio::spawn(async move {
                    while let Some(Ok(new_tx_state)) = pending_tx_receiver.next().await {
                        let subscription_request_metadata = RequestMetadata::new(
                            &app,
                            authorization.clone(),
                            RequestOrMethod::Method("eth_subscribe(newPendingFullTransactions)", 0),
                            None,
                        )
                        .await;

                        let new_tx = match new_tx_state {
                            TxStatus::Pending(tx) => tx,
                            TxStatus::Confirmed(..) => continue,
                            TxStatus::Orphaned(tx) => tx,
                        };

                        // TODO: make a struct for this? using our JsonRpcForwardedResponse won't work because it needs an id
                        let response_json = json!({
                            "jsonrpc": "2.0",
                            "method": "eth_subscription",
                            "params": {
                                "subscription": subscription_id,
                                // upstream just sends the txid, but we want to send the whole transaction
                                "result": new_tx,
                            },
                        });

                        subscription_request_metadata.add_response(&response_json);

                        let response_str = serde_json::to_string(&response_json)
                            .expect("this should always be valid json");

                        // TODO: do clients support binary messages?
                        let response_msg = Message::Text(response_str);

                        if response_sender.send_async(response_msg).await.is_err() {
                            // TODO: cancel this subscription earlier? select on head_block_receiver.next() and an abort handle?
                            break;
                        };
                    }

                    trace!(
                        "closed newPendingFullTransactions subscription: {:?}",
                        subscription_id
                    );
                });
            }
            Some(x) if x == &json!(["newPendingRawTransactions"]) => {
                // TODO: too much copy/pasta with newPendingTransactions
                let pending_tx_receiver = self.pending_tx_sender.subscribe();
                let app = self.clone();

                let mut pending_tx_receiver = Abortable::new(
                    BroadcastStream::new(pending_tx_receiver),
                    subscription_registration,
                );

                trace!(
                    "pending transactions subscription id: {:?}",
                    subscription_id
                );

                // TODO: do something with this handle?
                tokio::spawn(async move {
                    while let Some(Ok(new_tx_state)) = pending_tx_receiver.next().await {
                        let subscription_request_metadata = RequestMetadata::new(
                            &app,
                            authorization.clone(),
                            "eth_subscribe(newPendingRawTransactions)",
                            None,
                        )
                        .await;

                        let new_tx = match new_tx_state {
                            TxStatus::Pending(tx) => tx,
                            TxStatus::Confirmed(..) => continue,
                            TxStatus::Orphaned(tx) => tx,
                        };

                        // TODO: make a struct for this? using our JsonRpcForwardedResponse won't work because it needs an id
                        let response_json = json!({
                            "jsonrpc": "2.0",
                            "method": "eth_subscription",
                            "params": {
                                "subscription": subscription_id,
                                // upstream just sends the txid, but we want to send the raw transaction
                                "result": new_tx.rlp(),
                            },
                        });

                        let response_str = serde_json::to_string(&response_json)
                            .expect("this should always be valid json");

                        // we could use response.num_bytes() here, but since we already have the string, this is easier
                        let response_bytes = response_str.len();

                        // TODO: do clients support binary messages?
                        let response_msg = Message::Text(response_str);

                        if response_sender.send_async(response_msg).await.is_err() {
                            // TODO: cancel this subscription earlier? select on head_block_receiver.next() and an abort handle?
                            break;
                        };

                        subscription_request_metadata.add_response(response_bytes);
                    }

                    trace!(
                        "closed newPendingRawTransactions subscription: {:?}",
                        subscription_id
                    );
                });
            }
            _ => return Err(Web3ProxyError::NotImplemented),
        }

        // TODO: do something with subscription_join_handle?

        let response_data = JsonRpcResponseData::from(json!(subscription_id));

        let response = JsonRpcForwardedResponse::from_response_data(response_data, id);

        // TODO: this serializes twice
        request_metadata.add_response(&response);

        // TODO: make a `SubscriptonHandle(AbortHandle, JoinHandle)` struct?
        Ok((subscription_abort_handle, response))
    }
}
