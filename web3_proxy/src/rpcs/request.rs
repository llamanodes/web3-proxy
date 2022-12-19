use super::connection::Web3Connection;
use super::provider::Web3Provider;
use crate::frontend::authorization::{Authorization, AuthorizationType};
use crate::metered::{JsonRpcErrorCount, ProviderErrorCount};
use anyhow::Context;
use chrono::Utc;
use entities::revert_log;
use entities::sea_orm_active_enums::Method;
use ethers::providers::{HttpClientError, ProviderError, WsClientError};
use ethers::types::{Address, Bytes};
use log::{debug, error, trace, warn, Level};
use metered::metered;
use metered::HitCount;
use metered::ResponseTime;
use metered::Throughput;
use migration::sea_orm::{self, ActiveEnum, ActiveModelTrait};
use serde_json::json;
use std::fmt;
use std::sync::atomic::{self, AtomicBool, Ordering};
use std::sync::Arc;
use thread_fast_rng::rand::Rng;
use tokio::time::{sleep, Duration, Instant};

#[derive(Debug)]
pub enum OpenRequestResult {
    Handle(OpenRequestHandle),
    /// Unable to start a request. Retry at the given time.
    RetryAt(Instant),
    /// Unable to start a request because the server is not synced
    NotReady,
}

/// Make RPC requests through this handle and drop it when you are done.
#[derive(Debug)]
pub struct OpenRequestHandle {
    authorization: Arc<Authorization>,
    conn: Arc<Web3Connection>,
    // TODO: this is the same metrics on the conn. use a reference?
    metrics: Arc<OpenRequestHandleMetrics>,
    provider: Arc<Web3Provider>,
    used: AtomicBool,
}

/// Depending on the context, RPC errors can require different handling.
pub enum RequestErrorHandler {
    /// Log at the trace level. Use when errors are expected.
    TraceLevel,
    /// Log at the debug level. Use when errors are expected.
    DebugLevel,
    /// Log at the error level. Use when errors are bad.
    ErrorLevel,
    /// Log at the warn level. Use when errors do not cause problems.
    WarnLevel,
    /// Potentially save the revert. Users can tune how often this happens
    SaveReverts,
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
            Level::Trace => RequestErrorHandler::TraceLevel,
            Level::Debug => RequestErrorHandler::DebugLevel,
            Level::Error => RequestErrorHandler::ErrorLevel,
            Level::Warn => RequestErrorHandler::WarnLevel,
            _ => unimplemented!("unexpected tracing Level"),
        }
    }
}

impl Authorization {
    /// Save a RPC call that return "execution reverted" to the database.
    async fn save_revert(
        self: Arc<Self>,
        method: Method,
        params: EthCallFirstParams,
    ) -> anyhow::Result<()> {
        let rpc_key_id = match self.checks.rpc_key_id {
            Some(rpc_key_id) => rpc_key_id.into(),
            None => {
                // // trace!(?self, "cannot save revert without rpc_key_id");
                return Ok(());
            }
        };

        let db_conn = self.db_conn.as_ref().context("no database connection")?;

        // TODO: should the database set the timestamp?
        // we intentionally use "now" and not the time the request started
        // why? because we aggregate stats and setting one in the past could cause confusion
        let timestamp = Utc::now();
        let to: Vec<u8> = params
            .to
            .as_bytes()
            .try_into()
            .expect("address should always convert to a Vec<u8>");
        let call_data = params.data.map(|x| format!("{}", x));

        let rl = revert_log::ActiveModel {
            rpc_key_id: sea_orm::Set(rpc_key_id),
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
        trace!("revert_log: {:?}", rl);

        // TODO: return something useful
        Ok(())
    }
}

#[metered(registry = OpenRequestHandleMetrics, visibility = pub)]
impl OpenRequestHandle {
    pub async fn new(authorization: Arc<Authorization>, conn: Arc<Web3Connection>) -> Self {
        // TODO: take request_id as an argument?
        // TODO: attach a unique id to this? customer requests have one, but not internal queries
        // TODO: what ordering?!
        // TODO: should we be using metered, or not? i think not because we want stats for each handle
        // TODO: these should maybe be sent to an influxdb instance?
        conn.active_requests.fetch_add(1, atomic::Ordering::Relaxed);

        let mut provider = None;
        let mut logged = false;
        while provider.is_none() {
            // trace!("waiting on provider: locking...");

            let ready_provider = conn
                .provider_state
                .read()
                .await
                // TODO: hard code true, or take a bool in the `new` function?
                .provider(true)
                .await
                .cloned();
            // trace!("waiting on provider: unlocked!");

            match ready_provider {
                None => {
                    if !logged {
                        logged = true;
                        warn!("no provider for {}!", conn);
                    }

                    // TODO: how should this work? a reconnect should be in progress. but maybe force one now?
                    // TODO: sleep how long? subscribe to something instead? maybe use a watch handle?
                    // TODO: this is going to be way too verbose!
                    sleep(Duration::from_millis(100)).await
                }
                Some(x) => provider = Some(x),
            }
        }
        let provider = provider.expect("provider was checked already");

        // TODO: handle overflows?
        // TODO: what ordering?
        match authorization.as_ref().authorization_type {
            AuthorizationType::Frontend => {
                conn.frontend_requests
                    .fetch_add(1, atomic::Ordering::Relaxed);
            }
            AuthorizationType::Internal => {
                conn.internal_requests
                    .fetch_add(1, atomic::Ordering::Relaxed);
            }
        }

        let metrics = conn.open_request_handle_metrics.clone();
        let used = false.into();

        Self {
            authorization,
            conn,
            metrics,
            provider,
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
        // the authorization field is already on a parent span
        // trace!(rpc=%self.conn, %method, "request");

        // trace!("got provider for {:?}", self);

        // TODO: really sucks that we have to clone here
        let response = match &*self.provider {
            Web3Provider::Mock => unimplemented!(),
            Web3Provider::Http(provider) => provider.request(method, params).await,
            Web3Provider::Ws(provider) => provider.request(method, params).await,
        };

        // TODO: i think ethers already has trace logging (and does it much more fancy)
        trace!(
            "response from {} for {} {:?}: {:?}",
            self.conn,
            method,
            params,
            response,
        );

        if let Err(err) = &response {
            // only save reverts for some types of calls
            // TODO: do something special for eth_sendRawTransaction too
            let error_handler = if let RequestErrorHandler::SaveReverts = error_handler {
                // TODO: should all these be Trace or Debug or a mix?
                if !["eth_call", "eth_estimateGas"].contains(&method) {
                    // trace!(%method, "skipping save on revert");
                    RequestErrorHandler::TraceLevel
                } else if self.authorization.db_conn.is_some() {
                    let log_revert_chance = self.authorization.checks.log_revert_chance;

                    if log_revert_chance == 0.0 {
                        // trace!(%method, "no chance. skipping save on revert");
                        RequestErrorHandler::TraceLevel
                    } else if log_revert_chance == 1.0 {
                        // trace!(%method, "gaurenteed chance. SAVING on revert");
                        error_handler
                    } else if thread_fast_rng::thread_fast_rng().gen_range(0.0f64..=1.0)
                        < log_revert_chance
                    {
                        // trace!(%method, "missed chance. skipping save on revert");
                        RequestErrorHandler::TraceLevel
                    } else {
                        // trace!("Saving on revert");
                        // TODO: is always logging at debug level fine?
                        error_handler
                    }
                } else {
                    // trace!(%method, "no database. skipping save on revert");
                    RequestErrorHandler::TraceLevel
                }
            } else {
                error_handler
            };

            // check for "execution reverted" here
            let is_revert = if let ProviderError::JsonRpcClientError(err) = err {
                // Http and Ws errors are very similar, but different types
                let msg = match &*self.provider {
                    Web3Provider::Mock => unimplemented!(),
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
                    msg.starts_with("execution reverted")
                } else {
                    false
                }
            } else {
                false
            };

            if is_revert {
                trace!("revert from {}", self.conn);
            }

            // TODO: think more about the method and param logs. those can be sensitive information
            match error_handler {
                RequestErrorHandler::DebugLevel => {
                    // TODO: think about this revert check more. sometimes we might want reverts logged so this needs a flag
                    if !is_revert {
                        debug!(
                            "bad response from {}! method={} params={:?} err={:?}",
                            self.conn, method, params, err
                        );
                    }
                }
                RequestErrorHandler::TraceLevel => {
                    trace!(
                        "bad response from {}! method={} params={:?} err={:?}",
                        self.conn,
                        method,
                        params,
                        err
                    );
                }
                RequestErrorHandler::ErrorLevel => {
                    // TODO: include params if not running in release mode
                    error!(
                        "bad response from {}! method={} err={:?}",
                        self.conn, method, err
                    );
                }
                RequestErrorHandler::WarnLevel => {
                    // TODO: include params if not running in release mode
                    warn!(
                        "bad response from {}! method={} err={:?}",
                        self.conn, method, err
                    );
                }
                RequestErrorHandler::SaveReverts => {
                    trace!(
                        "bad response from {}! method={} params={:?} err={:?}",
                        self.conn,
                        method,
                        params,
                        err
                    );

                    // TODO: do not unwrap! (doesn't matter much since we check method as a string above)
                    let method: Method = Method::try_from_value(&method.to_string()).unwrap();

                    // TODO: DO NOT UNWRAP! But also figure out the best way to keep returning ProviderErrors here
                    let params: EthCallParams = serde_json::from_value(json!(params))
                        .context("parsing params to EthCallParams")
                        .unwrap();

                    // spawn saving to the database so we don't slow down the request
                    let f = self.authorization.clone().save_revert(method, params.0 .0);

                    tokio::spawn(f);
                }
            }
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
