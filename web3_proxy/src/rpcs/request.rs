use super::one::Web3Rpc;
use crate::errors::{Web3ProxyErrorContext, Web3ProxyResult};
use crate::frontend::authorization::{Authorization, AuthorizationType};
use crate::jsonrpc::{JsonRpcParams, JsonRpcResultData};
use anyhow::Context;
use chrono::Utc;
use derive_more::From;
use entities::revert_log;
use entities::sea_orm_active_enums::Method;
use ethers::providers::ProviderError;
use ethers::types::{Address, Bytes};
use migration::sea_orm::{self, ActiveEnum, ActiveModelTrait};
use nanorand::Rng;
use serde_json::json;
use std::sync::atomic;
use std::sync::Arc;
use tokio::time::{Duration, Instant};
use tracing::{debug, error, info, trace, warn, Level};

#[derive(Debug, From)]
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
    error_handler: RequestErrorHandler,
    rpc: Arc<Web3Rpc>,
}

/// Depending on the context, RPC errors require different handling.
#[derive(Copy, Clone, Debug, Default)]
pub enum RequestErrorHandler {
    /// Log at the trace level. Use when errors are expected.
    #[default]
    TraceLevel,
    /// Log at the debug level. Use when errors are expected.
    DebugLevel,
    /// Log at the info level. Use when errors are expected.
    InfoLevel,
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
    to: Option<Address>,
    data: Option<Bytes>,
}

impl From<Level> for RequestErrorHandler {
    fn from(level: Level) -> Self {
        match level {
            Level::DEBUG => RequestErrorHandler::DebugLevel,
            Level::ERROR => RequestErrorHandler::ErrorLevel,
            Level::INFO => RequestErrorHandler::InfoLevel,
            Level::TRACE => RequestErrorHandler::TraceLevel,
            Level::WARN => RequestErrorHandler::WarnLevel,
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

        let to = params.to.unwrap_or_else(Address::zero).as_bytes().to_vec();

        let call_data = params.data.map(|x| x.to_string());

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
            .web3_context("Failed saving new revert log")?;

        // TODO: what log level and format?
        trace!(revert_log=?rl);

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
    pub async fn new(
        authorization: Arc<Authorization>,
        rpc: Arc<Web3Rpc>,
        error_handler: Option<RequestErrorHandler>,
    ) -> Self {
        // TODO: take request_id as an argument?
        // TODO: attach a unique id to this? customer requests have one, but not internal queries
        // TODO: what ordering?!
        rpc.active_requests
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);

        let error_handler = error_handler.unwrap_or_default();

        Self {
            authorization,
            error_handler,
            rpc,
        }
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
    pub async fn request<P: JsonRpcParams, R: JsonRpcResultData + serde::Serialize>(
        self,
        method: &str,
        params: &P,
    ) -> Result<R, ProviderError> {
        // TODO: use tracing spans
        // TODO: including params in this log is way too verbose
        // trace!(rpc=%self.rpc, %method, "request");
        trace!("requesting from {}", self.rpc);

        match self.authorization.authorization_type {
            AuthorizationType::Frontend => {
                self.rpc
                    .external_requests
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            AuthorizationType::Internal => {
                self.rpc
                    .internal_requests
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }

        // we used to fetch_add the active_request count here, but sometimes a request is made without going through this function (like with subscriptions)

        let start = Instant::now();

        // TODO: replace ethers-rs providers with our own that supports streaming the responses
        // TODO: replace ethers-rs providers with our own that handles "id" being null
        let response: Result<R, _> = if let Some(ref p) = self.rpc.http_provider {
            p.request(method, params).await
        } else if let Some(p) = self.rpc.ws_provider.load().as_ref() {
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
            let error_handler = if let RequestErrorHandler::Save = self.error_handler {
                // TODO: should all these be Trace or Debug or a mix?
                if !["eth_call", "eth_estimateGas"].contains(&method) {
                    // trace!(%method, "skipping save on revert");
                    RequestErrorHandler::TraceLevel
                } else if self.authorization.db_conn.is_some() {
                    let log_revert_chance = self.authorization.checks.log_revert_chance;

                    if log_revert_chance == 0 {
                        // trace!(%method, "no chance. skipping save on revert");
                        RequestErrorHandler::TraceLevel
                    } else if log_revert_chance == u16::MAX {
                        // trace!(%method, "gaurenteed chance. SAVING on revert");
                        self.error_handler
                    } else if nanorand::tls_rng().generate_range(0u16..u16::MAX) < log_revert_chance
                    {
                        // trace!(%method, "missed chance. skipping save on revert");
                        RequestErrorHandler::TraceLevel
                    } else {
                        // trace!("Saving on revert");
                        // TODO: is always logging at debug level fine?
                        self.error_handler
                    }
                } else {
                    // trace!(%method, "no database. skipping save on revert");
                    RequestErrorHandler::TraceLevel
                }
            } else {
                self.error_handler
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
                // JsonRpc and Application errors get rolled into the JsonRpcClientError
                let msg = err.as_error_response().map(|x| x.message.clone());

                if let Some(msg) = msg {
                    trace!(%msg, "jsonrpc error message");

                    if msg.starts_with("execution reverted") {
                        trace!("revert from {}", self.rpc);
                        ResponseTypes::Revert
                    } else if msg.contains("limit") || msg.contains("request") {
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
                    let retry_at = Instant::now() + Duration::from_secs(1);

                    if self.rpc.backup {
                        debug!(?retry_at, "rate limited on {}!", self.rpc);
                    } else {
                        warn!(?retry_at, "rate limited on {}!", self.rpc);
                    }

                    hard_limit_until.send_replace(retry_at);
                }
            }

            // TODO: think more about the method and param logs. those can be sensitive information
            // we do **NOT** use self.error_handler here because it might have been modified
            match error_handler {
                RequestErrorHandler::DebugLevel => {
                    // TODO: think about this revert check more. sometimes we might want reverts logged so this needs a flag
                    if matches!(response_type, ResponseTypes::Revert) {
                        trace!(
                            rpc=%self.rpc,
                            %method,
                            ?params,
                            ?err,
                            "revert",
                        );
                    } else {
                        debug!(
                            rpc=%self.rpc,
                            %method,
                            ?params,
                            ?err,
                            "bad response",
                        );
                    }
                }
                RequestErrorHandler::InfoLevel => {
                    info!(
                        rpc=%self.rpc,
                        %method,
                        ?params,
                        ?err,
                        "bad response",
                    );
                }
                RequestErrorHandler::TraceLevel => {
                    trace!(
                        rpc=%self.rpc,
                        %method,
                        ?params,
                        ?err,
                        "bad response",
                    );
                }
                RequestErrorHandler::ErrorLevel => {
                    // TODO: only include params if not running in release mode
                    error!(
                        rpc=%self.rpc,
                        %method,
                        ?params,
                        ?err,
                        "bad response",
                    );
                }
                RequestErrorHandler::WarnLevel => {
                    // TODO: only include params if not running in release mode
                    warn!(
                        rpc=%self.rpc,
                        %method,
                        ?params,
                        ?err,
                        "bad response",
                    );
                }
                RequestErrorHandler::Save => {
                    trace!(
                        rpc=%self.rpc,
                        %method,
                        ?params,
                        ?err,
                        "bad response",
                    );

                    // TODO: do not unwrap! (doesn't matter much since we check method as a string above)
                    let method: Method = Method::try_from_value(&method.to_string()).unwrap();

                    // TODO: i don't think this prsing is correct
                    match serde_json::from_value::<EthCallParams>(json!(params)) {
                        Ok(params) => {
                            // spawn saving to the database so we don't slow down the request
                            let f = self.authorization.clone().save_revert(method, params.0 .0);

                            tokio::spawn(f);
                        }
                        Err(err) => {
                            warn!(
                                %method,
                                ?params,
                                ?err,
                                "failed parsing eth_call params. unable to save revert",
                            );
                        }
                    }
                }
            }
        }

        tokio::spawn(async move {
            self.rpc.peak_latency.as_ref().unwrap().report(latency);
            self.rpc.median_latency.as_ref().unwrap().record(latency);
        });

        response
    }
}
