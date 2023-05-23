use super::one::Web3Rpc;
use crate::frontend::authorization::Authorization;
use crate::frontend::errors::Web3ProxyResult;
use anyhow::Context;
use chrono::Utc;
use entities::revert_log;
use entities::sea_orm_active_enums::Method;
use ethers::providers::ProviderError;
use ethers::types::{Address, Bytes};
use log::{debug, error, trace, warn, Level};
use migration::sea_orm::{self, ActiveEnum, ActiveModelTrait};
use serde_json::json;
use std::fmt;
use std::sync::atomic;
use std::sync::Arc;
use thread_fast_rng::rand::Rng;
use tokio::time::{Duration, Instant};

#[derive(Debug)]
pub enum OpenRequestResult {
    Handle(OpenRequestHandle),
    /// Unable to start a request. Retry at the given time.
    RetryAt(Instant),
    /// Unable to start a request because no servers are synced
    NotReady,
}

/// Make RPC requests through this handle and drop it when you are done.
/// Opening this handle checks rate limits. Developers, try to keep opening a handle and using it as close together as possible
#[derive(Debug)]
pub struct OpenRequestHandle {
    authorization: Arc<Authorization>,
    rpc: Arc<Web3Rpc>,
}

/// Depending on the context, RPC errors require different handling.
#[derive(Copy, Clone)]
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
    Save,
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
    ) -> Web3ProxyResult<()> {
        let rpc_key_id = match self.checks.rpc_secret_key_id {
            Some(rpc_key_id) => rpc_key_id.into(),
            None => {
                // trace!(?self, "cannot save revert without rpc_key_id");
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

impl Drop for OpenRequestHandle {
    fn drop(&mut self) {
        self.rpc
            .active_requests
            .fetch_sub(1, atomic::Ordering::AcqRel);
    }
}

impl OpenRequestHandle {
    pub async fn new(authorization: Arc<Authorization>, rpc: Arc<Web3Rpc>) -> Self {
        // TODO: take request_id as an argument?
        // TODO: attach a unique id to this? customer requests have one, but not internal queries
        // TODO: what ordering?!
        rpc.active_requests
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);

        Self { authorization, rpc }
    }

    pub fn connection_name(&self) -> String {
        self.rpc.name.clone()
    }

    #[inline]
    pub fn clone_connection(&self) -> Arc<Web3Rpc> {
        self.rpc.clone()
    }

    /// Send a web3 request
    /// By having the request method here, we ensure that the rate limiter was called and connection counts were properly incremented
    /// depending on how things are locked, you might need to pass the provider in
    /// we take self to ensure this function only runs once
    pub async fn request<P, R>(
        self,
        method: &str,
        params: &P,
        mut error_handler: RequestErrorHandler,
    ) -> Result<R, ProviderError>
    where
        // TODO: not sure about this type. would be better to not need clones, but measure and spawns combine to need it
        P: Clone + fmt::Debug + serde::Serialize + Send + Sync + 'static,
        R: serde::Serialize + serde::de::DeserializeOwned + fmt::Debug + Send,
    {
        // TODO: use tracing spans
        // TODO: including params in this log is way too verbose
        // trace!(rpc=%self.rpc, %method, "request");
        trace!("requesting from {}", self.rpc);

        self.rpc
            .total_requests
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);

        // we used to fetch_add the active_request count here, but sometimes a request is made without going through this function (like with subscriptions)

        let start = Instant::now();

        // TODO: replace ethers-rs providers with our own that supports streaming the responses
        // TODO: replace ethers-rs providers with our own that handles "id" being null
        let response: Result<R, _> = if let Some(ref p) = self.rpc.http_provider {
            p.request(method, params).await
        } else if let Some(ref p) = self.rpc.ws_provider {
            p.request(method, params).await
        } else {
            return Err(ProviderError::CustomError(
                "no provider configured!".to_string(),
            ));
        };

        // we do NOT want to measure errors, so we intentionally do not record this latency now.
        let latency = start.elapsed();

        // we used to fetch_sub the active_request count here, but sometimes the handle is dropped without request being called!

        trace!(
            "response from {} for {} {:?}: {:?}",
            self.rpc,
            method,
            params,
            response,
        );

        if let Err(err) = &response {
            // only save reverts for some types of calls
            // TODO: do something special for eth_sendRawTransaction too
            error_handler = if let RequestErrorHandler::Save = error_handler {
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

            // TODO: simple enum -> string derive?
            // TODO: if ProviderError::UnsupportedRpc, we should retry on another server
            #[derive(Debug)]
            enum ResponseTypes {
                Revert,
                RateLimit,
                Error,
            }

            // check for "execution reverted" here
            // TODO: move this info a function on ResponseErrorType
            let response_type = if let ProviderError::JsonRpcClientError(err) = err {
                // Http and Ws errors are very similar, but different types
                let msg = err.as_error_response().map(|x| x.message.clone());

                trace!("error message: {:?}", msg);

                if let Some(msg) = msg {
                    if msg.starts_with("execution reverted") {
                        trace!("revert from {}", self.rpc);
                        ResponseTypes::Revert
                    } else if msg.contains("limit") || msg.contains("request") {
                        // TODO: too verbose
                        if self.rpc.backup {
                            trace!("rate limit from {}", self.rpc);
                        } else {
                            warn!("rate limit from {}", self.rpc);
                        }
                        ResponseTypes::RateLimit
                    } else {
                        ResponseTypes::Error
                    }
                } else {
                    ResponseTypes::Error
                }
            } else {
                ResponseTypes::Error
            };

            if matches!(response_type, ResponseTypes::RateLimit) {
                if let Some(hard_limit_until) = self.rpc.hard_limit_until.as_ref() {
                    // TODO: how long should we actually wait? different providers have different times
                    // TODO: if rate_limit_period_seconds is set, use that
                    // TODO: check response headers for rate limits too
                    // TODO: warn if production, debug if backup
                    if self.rpc.backup {
                        debug!("unexpected rate limit on {}!", self.rpc);
                    } else {
                        warn!("unexpected rate limit on {}!", self.rpc);
                    }

                    let retry_at = Instant::now() + Duration::from_secs(1);

                    trace!("retry {} at: {:?}", self.rpc, retry_at);

                    hard_limit_until.send_replace(retry_at);
                }
            }

            // TODO: think more about the method and param logs. those can be sensitive information
            match error_handler {
                RequestErrorHandler::DebugLevel => {
                    // TODO: think about this revert check more. sometimes we might want reverts logged so this needs a flag
                    if matches!(response_type, ResponseTypes::Revert) {
                        debug!(
                            "bad response from {}! method={} params={:?} err={:?}",
                            self.rpc, method, params, err
                        );
                    }
                }
                RequestErrorHandler::TraceLevel => {
                    trace!(
                        "bad response from {}! method={} params={:?} err={:?}",
                        self.rpc,
                        method,
                        params,
                        err
                    );
                }
                RequestErrorHandler::ErrorLevel => {
                    // TODO: include params if not running in release mode
                    error!(
                        "bad response from {}! method={} err={:?}",
                        self.rpc, method, err
                    );
                }
                RequestErrorHandler::WarnLevel => {
                    // TODO: include params if not running in release mode
                    warn!(
                        "bad response from {}! method={} err={:?}",
                        self.rpc, method, err
                    );
                }
                RequestErrorHandler::Save => {
                    trace!(
                        "bad response from {}! method={} params={:?} err={:?}",
                        self.rpc,
                        method,
                        params,
                        err
                    );

                    // TODO: do not unwrap! (doesn't matter much since we check method as a string above)
                    let method: Method = Method::try_from_value(&method.to_string()).unwrap();

                    match serde_json::from_value::<EthCallParams>(json!(params)) {
                        Ok(params) => {
                            // spawn saving to the database so we don't slow down the request
                            let f = self.authorization.clone().save_revert(method, params.0 .0);

                            tokio::spawn(f);
                        }
                        Err(err) => {
                            warn!(
                                "failed parsing eth_call params. unable to save revert. {}",
                                err
                            );
                        }
                    }
                }
            }
        } else if let Some(peak_latency) = &self.rpc.peak_latency {
            peak_latency.report(latency);
        } else {
            unreachable!("peak_latency not initialized");
        }

        response
    }
}
