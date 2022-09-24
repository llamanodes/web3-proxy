use super::connection::Web3Connection;
use super::provider::Web3Provider;
use crate::frontend::authorization::AuthorizedRequest;
use crate::metered::{JsonRpcErrorCount, ProviderErrorCount};
use anyhow::Context;
use chrono::Utc;
use entities::revert_logs;
use entities::sea_orm_active_enums::Method;
use ethers::providers::{HttpClientError, ProviderError, WsClientError};
use ethers::types::{Address, Bytes};
use metered::metered;
use metered::HitCount;
use metered::ResponseTime;
use metered::Throughput;
use num_traits::cast::FromPrimitive;
use rand::Rng;
use sea_orm::prelude::Decimal;
use sea_orm::ActiveEnum;
use sea_orm::ActiveModelTrait;
use serde_json::json;
use std::fmt;
use std::sync::atomic::{self, AtomicBool, Ordering};
use std::sync::Arc;
use tokio::time::{sleep, Duration, Instant};
use tracing::Level;
use tracing::{debug, error, trace, warn};

#[derive(Debug)]
pub enum OpenRequestResult {
    Handle(OpenRequestHandle),
    /// Unable to start a request. Retry at the given time.
    RetryAt(Instant),
    /// Unable to start a request. Retrying will not succeed.
    RetryNever,
}

/// Make RPC requests through this handle and drop it when you are done.
#[derive(Debug)]
pub struct OpenRequestHandle {
    authorized_request: Arc<AuthorizedRequest>,
    conn: Arc<Web3Connection>,
    // TODO: this is the same metrics on the conn. use a reference?
    metrics: Arc<OpenRequestHandleMetrics>,
    used: AtomicBool,
}

/// Depending on the context, RPC errors can require different handling.
pub enum RequestErrorHandler {
    /// Potentially save the revert. Users can tune how often this happens
    SaveReverts,
    /// Log at the debug level. Use when errors are expected.
    DebugLevel,
    /// Log at the error level. Use when errors are bad.
    ErrorLevel,
    /// Log at the warn level. Use when errors do not cause problems.
    WarnLevel,
}

// TODO: second param could be skipped since we don't need it here
#[derive(serde::Deserialize, serde::Serialize)]
struct EthCallParams((EthCallFirstParams, Option<serde_json::Value>));

#[derive(serde::Deserialize, serde::Serialize)]
struct EthCallFirstParams {
    to: Address,
    data: Option<Bytes>,
}

impl From<Level> for RequestErrorHandler {
    fn from(level: Level) -> Self {
        match level {
            Level::DEBUG => RequestErrorHandler::DebugLevel,
            Level::ERROR => RequestErrorHandler::ErrorLevel,
            Level::WARN => RequestErrorHandler::WarnLevel,
            _ => unimplemented!("unexpected tracing Level"),
        }
    }
}

impl AuthorizedRequest {
    /// Save a RPC call that return "execution reverted" to the database.
    async fn save_revert(
        self: Arc<Self>,
        method: Method,
        params: EthCallFirstParams,
    ) -> anyhow::Result<()> {
        if let Self::User(Some(db_conn), authorized_request) = &*self {
            // TODO: should the database set the timestamp?
            let timestamp = Utc::now();
            let to: Vec<u8> = params
                .to
                .as_bytes()
                .try_into()
                .expect("address should always convert to a Vec<u8>");
            let call_data = params.data.map(|x| format!("{}", x));

            let rl = revert_logs::ActiveModel {
                user_key_id: sea_orm::Set(authorized_request.user_key_id),
                method: sea_orm::Set(method),
                to: sea_orm::Set(to),
                call_data: sea_orm::Set(call_data),
                timestamp: sea_orm::Set(timestamp),
                ..Default::default()
            };

            let rl = rl
                .save(db_conn)
                .await
                .context("Failed saving new revert log")?;

            // TODO: what log level?
            // TODO: better format
            trace!(?rl);
        }

        // TODO: return something useful
        Ok(())
    }
}

#[metered(registry = OpenRequestHandleMetrics, visibility = pub)]
impl OpenRequestHandle {
    pub fn new(
        conn: Arc<Web3Connection>,
        authorized_request: Option<Arc<AuthorizedRequest>>,
    ) -> Self {
        // TODO: take request_id as an argument?
        // TODO: attach a unique id to this? customer requests have one, but not internal queries
        // TODO: what ordering?!
        // TODO: should we be using metered, or not? i think not because we want stats for each handle
        // TODO: these should maybe be sent to an influxdb instance?
        conn.active_requests.fetch_add(1, atomic::Ordering::Relaxed);

        // TODO: handle overflows?
        // TODO: what ordering?
        conn.total_requests.fetch_add(1, atomic::Ordering::Relaxed);

        let metrics = conn.open_request_handle_metrics.clone();
        let used = false.into();

        let authorized_request =
            authorized_request.unwrap_or_else(|| Arc::new(AuthorizedRequest::Internal));

        Self {
            authorized_request,
            conn,
            metrics,
            used,
        }
    }

    #[inline]
    pub fn clone_connection(&self) -> Arc<Web3Connection> {
        self.conn.clone()
    }

    /// Send a web3 request
    /// By having the request method here, we ensure that the rate limiter was called and connection counts were properly incremented
    /// TODO: we no longer take self because metered doesn't like that
    /// TODO: ErrorCount includes too many types of errors, such as transaction reverts
    #[measure([JsonRpcErrorCount, HitCount, ProviderErrorCount, ResponseTime, Throughput])]
    pub async fn request<P, R>(
        &self,
        method: &str,
        params: &P,
        error_handler: RequestErrorHandler,
    ) -> Result<R, ProviderError>
    where
        // TODO: not sure about this type. would be better to not need clones, but measure and spawns combine to need it
        P: Clone + fmt::Debug + serde::Serialize + Send + Sync + 'static,
        R: serde::Serialize + serde::de::DeserializeOwned + fmt::Debug,
    {
        // ensure this function only runs once
        if self.used.swap(true, Ordering::Release) {
            unimplemented!("a request handle should only be used once");
        }

        // TODO: use tracing spans
        // TODO: requests from customers have request ids, but we should add
        // TODO: including params in this is way too verbose
        // the authorized_request field is already on a parent span
        trace!(rpc=%self.conn, %method, "request");

        let mut provider = None;

        while provider.is_none() {
            match self.conn.provider.read().await.clone() {
                None => {
                    warn!(rpc=%self.conn, "no provider!");
                    // TODO: how should this work? a reconnect should be in progress. but maybe force one now?
                    // TODO: sleep how long? subscribe to something instead? maybe use a watch handle?
                    // TODO: this is going to be way too verbose!
                    sleep(Duration::from_millis(100)).await
                }
                Some(found_provider) => provider = Some(found_provider),
            }
        }

        let provider = &*provider.expect("provider was checked already");

        // TODO: really sucks that we have to clone here
        let response = match provider {
            Web3Provider::Http(provider) => provider.request(method, params).await,
            Web3Provider::Ws(provider) => provider.request(method, params).await,
        };

        if let Err(err) = &response {
            // only save reverts for some types of calls
            // TODO: do something special for eth_sendRawTransaction too
            let error_handler = if let RequestErrorHandler::SaveReverts = error_handler {
                if !["eth_call", "eth_estimateGas"].contains(&method) {
                    trace!(%method, "skipping save on revert");
                    RequestErrorHandler::DebugLevel
                } else if self.authorized_request.db_conn().is_none() {
                    trace!(%method, "no database. skipping save on revert");
                    RequestErrorHandler::DebugLevel
                } else if let AuthorizedRequest::User(db_conn, y) = self.authorized_request.as_ref()
                {
                    if db_conn.is_none() {
                        trace!(%method, "no database. skipping save on revert");
                        RequestErrorHandler::DebugLevel
                    } else {
                        let log_revert_chance = y.log_revert_chance;

                        if log_revert_chance.is_zero() {
                            trace!(%method, "no chance. skipping save on revert");
                            RequestErrorHandler::DebugLevel
                        } else if log_revert_chance == Decimal::ONE {
                            trace!(%method, "gaurenteed chance. SAVING on revert");
                            error_handler
                        } else if Decimal::from_f32(rand::thread_rng().gen_range(0.0f32..=1.0))
                            .expect("f32 should always convert to a Decimal")
                            > log_revert_chance
                        {
                            trace!(%method, "missed chance. skipping save on revert");
                            RequestErrorHandler::DebugLevel
                        } else {
                            trace!("Saving on revert");
                            // TODO: is always logging at debug level fine?
                            error_handler
                        }
                    }
                } else {
                    RequestErrorHandler::DebugLevel
                }
            } else {
                error_handler
            };

            match error_handler {
                RequestErrorHandler::DebugLevel => {
                    debug!(?err, %method, rpc=%self.conn, "bad response!");
                }
                RequestErrorHandler::ErrorLevel => {
                    error!(?err, %method, rpc=%self.conn, "bad response!");
                }
                RequestErrorHandler::WarnLevel => {
                    warn!(?err, %method, rpc=%self.conn, "bad response!");
                }
                RequestErrorHandler::SaveReverts => {
                    // TODO: logging every one is going to flood the database
                    // TODO: have a percent chance to do this. or maybe a "logged reverts per second"
                    if let ProviderError::JsonRpcClientError(err) = err {
                        // Http and Ws errors are very similar, but different types
                        let msg = match provider {
                            Web3Provider::Http(_) => {
                                if let Some(HttpClientError::JsonRpcError(err)) =
                                    err.downcast_ref::<HttpClientError>()
                                {
                                    Some(&err.message)
                                } else {
                                    None
                                }
                            }
                            Web3Provider::Ws(_) => {
                                if let Some(WsClientError::JsonRpcError(err)) =
                                    err.downcast_ref::<WsClientError>()
                                {
                                    Some(&err.message)
                                } else {
                                    None
                                }
                            }
                        };

                        if let Some(msg) = msg {
                            if msg.starts_with("execution reverted") {
                                // TODO: do not unwrap! (doesn't matter much since we check method as a string above)
                                let method: Method =
                                    Method::try_from_value(&method.to_string()).unwrap();

                                // TODO: DO NOT UNWRAP! But also figure out the best way to keep returning ProviderErrors here
                                let params: EthCallParams = serde_json::from_value(json!(params))
                                    .context("parsing params to EthCallParams")
                                    .unwrap();

                                // spawn saving to the database so we don't slow down the request
                                let f = self
                                    .authorized_request
                                    .clone()
                                    .save_revert(method, params.0 .0);

                                tokio::spawn(async move { f.await });
                            } else {
                                // TODO: log any of the errors?
                                debug!(?err, %method, rpc=%self.conn, "bad response!");
                            }
                        }
                    }
                }
            }
        } else {
            // TODO: i think ethers already has trace logging (and does it much more fancy)
            // TODO: opt-in response inspection to log reverts with their request. put into redis or what?
            // trace!(rpc=%self.conn, %method, ?response);
            trace!(%method, rpc=%self.conn, "response");
        }

        response
    }
}

impl Drop for OpenRequestHandle {
    fn drop(&mut self) {
        self.conn
            .active_requests
            .fetch_sub(1, atomic::Ordering::AcqRel);
    }
}
