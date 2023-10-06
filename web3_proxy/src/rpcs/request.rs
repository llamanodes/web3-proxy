use super::one::Web3Rpc;
use crate::errors::{Web3ProxyError, Web3ProxyErrorContext, Web3ProxyResult};
use crate::frontend::authorization::{Authorization, AuthorizationType, Web3Request};
use crate::globals::{global_db_conn, DB_CONN};
use crate::jsonrpc::{self, JsonRpcErrorData, JsonRpcResultData, Payload};
use anyhow::Context;
use chrono::Utc;
use derive_more::From;
use entities::revert_log;
use entities::sea_orm_active_enums::Method;
use ethers::providers::ProviderError;
use ethers::types::{Address, Bytes};
use http::StatusCode;
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
pub struct OpenRequestHandle {
    web3_request: Arc<Web3Request>,
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

impl std::fmt::Debug for OpenRequestHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenRequestHandle")
            .field("method", &self.web3_request.inner.method())
            .field("rpc", &self.rpc.name)
            .finish_non_exhaustive()
    }
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

        let db_conn = global_db_conn()?;

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
            .save(&db_conn)
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
            .fetch_sub(1, atomic::Ordering::Relaxed);
    }
}

impl OpenRequestHandle {
    pub async fn new(
        web3_request: Arc<Web3Request>,
        rpc: Arc<Web3Rpc>,
        error_handler: Option<RequestErrorHandler>,
    ) -> Self {
        // TODO: take request_id as an argument?
        // TODO: attach a unique id to this? customer requests have one, but not internal queries
        // TODO: what ordering?!
        rpc.active_requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let error_handler = error_handler.unwrap_or_default();

        Self {
            web3_request,
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

    pub fn rate_limit_for(&self, x: Duration) {
        // TODO: we actually only want to send if our value is greater

        if self.rpc.backup {
            debug!(?x, "rate limited on {}!", self.rpc);
        } else {
            warn!(?x, "rate limited on {}!", self.rpc);
        }

        self.rpc
            .hard_limit_until
            .as_ref()
            .unwrap()
            .send_replace(Instant::now() + x);
    }

    /// Just get the response from the provider without any extra handling.
    /// This lets us use the try operator which makes it much easier to read
    async fn _request<R: JsonRpcResultData + serde::Serialize>(
        &self,
    ) -> Web3ProxyResult<jsonrpc::SingleResponse<R>> {
        // TODO: replace ethers-rs providers with our own that supports streaming the responses
        // TODO: replace ethers-rs providers with our own that handles "id" being null
        if let (Some(url), Some(ref client)) = (self.rpc.http_url.clone(), &self.rpc.http_client) {
            // prefer the http provider
            let request = self
                .web3_request
                .inner
                .jsonrpc_request()
                .context("there should always be a request here")?;

            let response = client.post(url).json(request).send().await?;

            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                // TODO: how much should we actually rate limit?
                self.rate_limit_for(Duration::from_secs(1));
            }

            let response = response.error_for_status()?;

            jsonrpc::SingleResponse::read_if_short(response, 1024, &self.web3_request).await
        } else if let Some(p) = self.rpc.ws_provider.load().as_ref() {
            // use the websocket provider if no http provider is available
            let method = self.web3_request.inner.method();
            let params = self.web3_request.inner.params();

            // some ethers::ProviderError need to be converted to JsonRpcErrorData. the rest to Web3ProxyError
            let response = match p.request::<_, R>(method, params).await {
                Ok(x) => jsonrpc::ParsedResponse::from_result(x, self.web3_request.id()),
                Err(provider_error) => match JsonRpcErrorData::try_from(&provider_error) {
                    Ok(x) => jsonrpc::ParsedResponse::from_error(x, self.web3_request.id()),
                    Err(ProviderError::HTTPError(error)) => {
                        if let Some(status_code) = error.status() {
                            if status_code == StatusCode::TOO_MANY_REQUESTS {
                                // TODO: how much should we actually rate limit?
                                self.rate_limit_for(Duration::from_secs(1));
                            }
                        }
                        return Err(provider_error.into());
                    }
                    Err(err) => {
                        warn!(?err, "error from {}", self.rpc);

                        return Err(provider_error.into());
                    }
                },
            };

            Ok(response.into())
        } else {
            // this must be a test
            Err(anyhow::anyhow!("no provider configured!").into())
        }
    }

    pub fn error_handler(&self) -> RequestErrorHandler {
        if let RequestErrorHandler::Save = self.error_handler {
            let method = self.web3_request.inner.method();

            // TODO: should all these be Trace or Debug or a mix?
            // TODO: this list should come from config. other methods might be desired
            if !["eth_call", "eth_estimateGas"].contains(&method) {
                // trace!(%method, "skipping save on revert");
                RequestErrorHandler::TraceLevel
            } else if DB_CONN.read().is_ok() {
                let log_revert_chance = self.web3_request.authorization.checks.log_revert_chance;

                if log_revert_chance == 0 {
                    // trace!(%method, "no chance. skipping save on revert");
                    RequestErrorHandler::TraceLevel
                } else if log_revert_chance == u16::MAX {
                    // trace!(%method, "gaurenteed chance. SAVING on revert");
                    self.error_handler
                } else if nanorand::tls_rng().generate_range(0u16..u16::MAX) < log_revert_chance {
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
        }
    }

    /// Send a web3 request
    /// By having the request method here, we ensure that the rate limiter was called and connection counts were properly incremented
    /// depending on how things are locked, you might need to pass the provider in
    /// we take self to ensure this function only runs once
    /// This does some inspection of the response to check for non-standard errors and rate limiting to try to give a Web3ProxyError instead of an Ok
    pub async fn request<R: JsonRpcResultData + serde::Serialize>(
        self,
    ) -> Web3ProxyResult<jsonrpc::SingleResponse<R>> {
        // TODO: use tracing spans
        // TODO: including params in this log is way too verbose
        // trace!(rpc=%self.rpc, %method, "request");
        trace!("requesting from {}", self.rpc);

        let authorization = &self.web3_request.authorization;

        match &authorization.authorization_type {
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

        // we generally don't want to use the try operator. we might need to log errors
        let start = Instant::now();

        let mut response = self._request().await;

        // measure successes and errors
        // originally i thought we wouldn't want errors, but I think it's a more accurate number including all requests
        let latency = start.elapsed();

        // we used to fetch_sub the active_request count here, but sometimes the handle is dropped without request being called!

        trace!(
            "response from {} for {}: {:?}",
            self.rpc,
            self.web3_request,
            response,
        );

        // TODO: move this to a helper function?
        // true if we got a jsonrpc result. a jsonrpc error or other error is false.
        // TODO: counters for errors vs jsonrpc vs success?
        let response_is_success = match &response {
            Ok(jsonrpc::SingleResponse::Parsed(x)) => {
                matches!(&x.payload, Payload::Success { .. })
            }
            Ok(jsonrpc::SingleResponse::Stream(..)) => false,
            Err(_) => false,
        };

        if response_is_success {
            // only track latency for successful requests
            tokio::spawn(async move {
                self.rpc.peak_latency.as_ref().unwrap().report(latency);
                self.rpc.median_latency.as_ref().unwrap().record(latency);

                // TODO: app-wide median and peak latency?
            });
        } else {
            // only save reverts for some types of calls
            // TODO: do something special for eth_sendRawTransaction too
            // we do **NOT** use self.error_handler here because it might have been modified
            let error_handler = self.error_handler();

            enum ResponseType {
                Error,
                Revert,
                RateLimited,
            }

            let response_type: ResponseType = match &response {
                Ok(jsonrpc::SingleResponse::Parsed(x)) => match &x.payload {
                    Payload::Success { .. } => unreachable!(),
                    Payload::Error { error } => {
                        trace!(?error, "jsonrpc error data");

                        if error.message.starts_with("execution reverted") {
                            ResponseType::Revert
                        } else if error.code == StatusCode::TOO_MANY_REQUESTS.as_u16() as i64 {
                            ResponseType::RateLimited
                        } else {
                            // TODO! THIS HAS TOO MANY FALSE POSITIVES! Theres another spot in the code that checks for things.
                            // if error.message.contains("limit") || error.message.contains("request") {
                            //     self.rate_limit_for(Duration::from_secs(1));
                            // }

                            match error.code {
                                -32000 => {
                                    // TODO: regex?
                                    let archive_prefixes = [
                                        "header not found",
                                        "header for hash not found",
                                        "missing trie node",
                                    ];
                                    for prefix in archive_prefixes {
                                        if error.message.starts_with(prefix) {
                                            // TODO: what error?
                                            response = Err(Web3ProxyError::NoBlockNumberOrHash);
                                            break;
                                        }
                                    }
                                }
                                -32601 => {
                                    let error_msg = error.message.as_ref();

                                    // sometimes a provider does not support all rpc methods
                                    // we check other connections rather than returning the error
                                    // but sometimes the method is something that is actually unsupported,
                                    // so we save the response here to return it later

                                    // some providers look like this
                                    if (error_msg.starts_with("the method")
                                        && error_msg.ends_with("is not available"))
                                        || error_msg == "Method not found"
                                    {
                                        let method = self.web3_request.inner.method().to_string();

                                        response =
                                            Err(Web3ProxyError::MethodNotFound(method.into()))
                                    }
                                }
                                _ => {}
                            }

                            ResponseType::Error
                        }
                    }
                },
                Ok(jsonrpc::SingleResponse::Stream(..)) => unreachable!(),
                Err(_) => ResponseType::Error,
            };

            if matches!(response_type, ResponseType::RateLimited) {
                // TODO: how long?
                self.rate_limit_for(Duration::from_secs(1));
            }

            match error_handler {
                RequestErrorHandler::DebugLevel => {
                    // TODO: think about this revert check more. sometimes we might want reverts logged so this needs a flag
                    if matches!(response_type, ResponseType::Revert) {
                        trace!(
                            rpc=%self.rpc,
                            %self.web3_request,
                            ?response,
                            "revert",
                        );
                    } else {
                        debug!(
                            rpc=%self.rpc,
                            %self.web3_request,
                            ?response,
                            "bad response",
                        );
                    }
                }
                RequestErrorHandler::InfoLevel => {
                    info!(
                        rpc=%self.rpc,
                        %self.web3_request,
                        ?response,
                        "bad response",
                    );
                }
                RequestErrorHandler::TraceLevel => {
                    trace!(
                        rpc=%self.rpc,
                        %self.web3_request,
                        ?response,
                        "bad response",
                    );
                }
                RequestErrorHandler::ErrorLevel => {
                    // TODO: only include params if not running in release mode
                    error!(
                        rpc=%self.rpc,
                        %self.web3_request,
                        ?response,
                        "bad response",
                    );
                }
                RequestErrorHandler::WarnLevel => {
                    // TODO: only include params if not running in release mode
                    warn!(
                        rpc=%self.rpc,
                        %self.web3_request,
                        ?response,
                        "bad response",
                    );
                }
                RequestErrorHandler::Save => {
                    trace!(
                        rpc=%self.rpc,
                        %self.web3_request,
                        ?response,
                        "bad response",
                    );

                    // TODO: do not unwrap! (doesn't matter much since we check method as a string above)
                    // TODO: open this up for even more methods
                    let method: Method =
                        Method::try_from_value(&self.web3_request.inner.method().to_string())
                            .unwrap();

                    // TODO: i don't think this prsing is correct
                    match serde_json::from_value::<EthCallParams>(json!(self
                        .web3_request
                        .inner
                        .params()))
                    {
                        Ok(params) => {
                            // spawn saving to the database so we don't slow down the request
                            // TODO: log if this errors
                            // TODO: aren't the method and params already saved? this should just need the response
                            let f = authorization.clone().save_revert(method, params.0 .0);

                            tokio::spawn(f);
                        }
                        Err(err) => {
                            warn!(
                                %self.web3_request,
                                ?response,
                                ?err,
                                "failed parsing eth_call params. unable to save revert",
                            );
                        }
                    }
                }
            }
        }

        response
    }
}
