//! Websocket-specific functions for the Web3ProxyApp

use super::Web3ProxyApp;
use crate::frontend::authorization::{Authorization, RequestMetadata};
use crate::frontend::errors::{Web3ProxyError, Web3ProxyErrorContext, Web3ProxyResult};
use crate::jsonrpc::JsonRpcForwardedResponse;
use crate::jsonrpc::JsonRpcRequest;
use crate::rpcs::transactions::TxStatus;
use crate::stats::RpcQueryStats;
use axum::extract::ws::Message;
use ethers::prelude::U64;
use futures::future::AbortHandle;
use futures::future::Abortable;
use futures::stream::StreamExt;
use log::{trace, warn};
use serde_json::json;
use std::sync::atomic::{self, AtomicUsize};
use std::sync::Arc;
use tokio_stream::wrappers::{BroadcastStream, WatchStream};

impl Web3ProxyApp {
    // TODO: #[measure([ErrorCount, HitCount, ResponseTime, Throughput])]
    pub async fn eth_subscribe<'a>(
        self: &'a Arc<Self>,
        authorization: Arc<Authorization>,
        request_json: JsonRpcRequest,
        subscription_count: &'a AtomicUsize,
        // TODO: taking a sender for Message instead of the exact json we are planning to send feels wrong, but its easier for now
        response_sender: flume::Sender<Message>,
    ) -> Web3ProxyResult<(AbortHandle, JsonRpcForwardedResponse)> {
        // TODO: this is not efficient
        let request_bytes = serde_json::to_string(&request_json)
            .web3_context("finding request size")?
            .len();

        let request_metadata = Arc::new(RequestMetadata::new(request_bytes));

        let (subscription_abort_handle, subscription_registration) = AbortHandle::new_pair();

        // TODO: this only needs to be unique per connection. we don't need it globably unique
        let subscription_id = subscription_count.fetch_add(1, atomic::Ordering::SeqCst);
        let subscription_id = U64::from(subscription_id);

        // save the id so we can use it in the response
        let id = request_json.id.clone();

        // TODO: calling json! on every request is probably not fast. but we can only match against
        // TODO: i think we need a stricter EthSubscribeRequest type that JsonRpcRequest can turn into
        match request_json.params.as_ref() {
            Some(x) if x == &json!(["newHeads"]) => {
                let authorization = authorization.clone();
                let head_block_receiver = self.watch_consensus_head_receiver.clone();
                let stat_sender = self.stat_sender.clone();

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

                        // TODO: what should the payload for RequestMetadata be?
                        let request_metadata = Arc::new(RequestMetadata::new(0));

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

                        // we could use response.num_bytes() here, but since we already have the string, this is easier
                        let response_bytes = response_str.len();

                        // TODO: do clients support binary messages?
                        let response_msg = Message::Text(response_str);

                        if response_sender.send_async(response_msg).await.is_err() {
                            // TODO: cancel this subscription earlier? select on head_block_receiver.next() and an abort handle?
                            break;
                        };

                        if let Some(stat_sender) = stat_sender.as_ref() {
                            let response_stat = RpcQueryStats::new(
                                "eth_subscription(newHeads)".to_string(),
                                authorization.clone(),
                                request_metadata.clone(),
                                response_bytes,
                            );

                            if let Err(err) = stat_sender.send_async(response_stat.into()).await {
                                // TODO: what should we do?
                                warn!(
                                    "stat_sender failed inside newPendingTransactions: {:?}",
                                    err
                                );
                            }
                        }
                    }

                    trace!("closed newHeads subscription {:?}", subscription_id);
                });
            }
            Some(x) if x == &json!(["newPendingTransactions"]) => {
                let pending_tx_receiver = self.pending_tx_sender.subscribe();
                let stat_sender = self.stat_sender.clone();
                let authorization = authorization.clone();

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
                        let request_metadata = Arc::new(RequestMetadata::new(0));

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

                        // we could use response.num_bytes() here, but since we already have the string, this is easier
                        let response_bytes = response_str.len();

                        // TODO: do clients support binary messages?
                        let response_msg = Message::Text(response_str);

                        if response_sender.send_async(response_msg).await.is_err() {
                            // TODO: cancel this subscription earlier? select on head_block_receiver.next() and an abort handle?
                            break;
                        };

                        if let Some(stat_sender) = stat_sender.as_ref() {
                            let response_stat = RpcQueryStats::new(
                                "eth_subscription(newPendingTransactions)".to_string(),
                                authorization.clone(),
                                request_metadata.clone(),
                                response_bytes,
                            );

                            if let Err(err) = stat_sender.send_async(response_stat.into()).await {
                                // TODO: what should we do?
                                warn!(
                                    "stat_sender failed inside newPendingTransactions: {:?}",
                                    err
                                );
                            }
                        }
                    }

                    trace!(
                        "closed newPendingTransactions subscription: {:?}",
                        subscription_id
                    );
                });
            }
            Some(x) if x == &json!(["newPendingFullTransactions"]) => {
                // TODO: too much copy/pasta with newPendingTransactions
                let authorization = authorization.clone();
                let pending_tx_receiver = self.pending_tx_sender.subscribe();
                let stat_sender = self.stat_sender.clone();

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
                        let request_metadata = Arc::new(RequestMetadata::new(0));

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

                        if let Some(stat_sender) = stat_sender.as_ref() {
                            let response_stat = RpcQueryStats::new(
                                "eth_subscription(newPendingFullTransactions)".to_string(),
                                authorization.clone(),
                                request_metadata.clone(),
                                response_bytes,
                            );

                            if let Err(err) = stat_sender.send_async(response_stat.into()).await {
                                // TODO: what should we do?
                                warn!(
                                    "stat_sender failed inside newPendingFullTransactions: {:?}",
                                    err
                                );
                            }
                        }
                    }

                    trace!(
                        "closed newPendingFullTransactions subscription: {:?}",
                        subscription_id
                    );
                });
            }
            Some(x) if x == &json!(["newPendingRawTransactions"]) => {
                // TODO: too much copy/pasta with newPendingTransactions
                let authorization = authorization.clone();
                let pending_tx_receiver = self.pending_tx_sender.subscribe();
                let stat_sender = self.stat_sender.clone();

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
                        let request_metadata = Arc::new(RequestMetadata::new(0));

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

                        if let Some(stat_sender) = stat_sender.as_ref() {
                            let response_stat = RpcQueryStats::new(
                                "eth_subscription(newPendingRawTransactions)".to_string(),
                                authorization.clone(),
                                request_metadata.clone(),
                                response_bytes,
                            );

                            if let Err(err) = stat_sender.send_async(response_stat.into()).await {
                                // TODO: what should we do?
                                warn!(
                                    "stat_sender failed inside newPendingRawTransactions: {:?}",
                                    err
                                );
                            }
                        }
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

        let response = JsonRpcForwardedResponse::from_value(json!(subscription_id), id);

        if let Some(stat_sender) = self.stat_sender.as_ref() {
            let response_stat = RpcQueryStats::new(
                request_json.method.clone(),
                authorization.clone(),
                request_metadata,
                response.num_bytes(),
            );

            if let Err(err) = stat_sender.send_async(response_stat.into()).await {
                // TODO: what should we do?
                warn!("stat_sender failed inside websocket: {:?}", err);
            }
        }

        // TODO: make a `SubscriptonHandle(AbortHandle, JoinHandle)` struct?
        Ok((subscription_abort_handle, response))
    }
}
