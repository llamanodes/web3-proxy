use super::connection::Web3Connection;
use super::provider::Web3Provider;
use crate::frontend::authorization::AuthorizedRequest;
use crate::metered::{JsonRpcErrorCount, ProviderErrorCount};
use ethers::providers::{HttpClientError, ProviderError, WsClientError};
use metered::metered;
use metered::HitCount;
use metered::ResponseTime;
use metered::Throughput;
use rand::Rng;
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
    authorization: Arc<AuthorizedRequest>,
    conn: Arc<Web3Connection>,
    // TODO: this is the same metrics on the conn. use a reference?
    metrics: Arc<OpenRequestHandleMetrics>,
    used: AtomicBool,
}

/// Depending on the context, RPC errors can require different handling.
pub enum RequestErrorHandler {
    /// Contains the percent chance to save the revert
    SaveReverts(f32),
    /// Log at the debug level. Use when errors are expected.
    DebugLevel,
    /// Log at the error level. Use when errors are bad.
    ErrorLevel,
    /// Log at the warn level. Use when errors do not cause problems.
    WarnLevel,
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
    async fn save_revert<T>(self: Arc<Self>, method: String, params: T) -> anyhow::Result<()>
    where
        T: Clone + fmt::Debug + serde::Serialize + Send + Sync + 'static,
    {
        todo!("save the revert to the database");
    }
}

#[metered(registry = OpenRequestHandleMetrics, visibility = pub)]
impl OpenRequestHandle {
    pub fn new(conn: Arc<Web3Connection>, authorization: Option<Arc<AuthorizedRequest>>) -> Self {
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

        let authorization = authorization.unwrap_or_else(|| {
            let db_conn = conn.db_conn.clone();
            Arc::new(AuthorizedRequest::Internal(db_conn))
        });

        Self {
            authorization,
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
    pub async fn request<T, R>(
        &self,
        method: &str,
        params: &T,
        error_handler: RequestErrorHandler,
    ) -> Result<R, ProviderError>
    where
        // TODO: not sure about this type. would be better to not need clones, but measure and spawns combine to need it
        T: Clone + fmt::Debug + serde::Serialize + Send + Sync + 'static,
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
            let error_handler = if let RequestErrorHandler::SaveReverts(save_chance) = error_handler
            {
                if ["eth_call", "eth_estimateGas"].contains(&method)
                    && self.authorization.has_db()
                    && (save_chance == 1.0
                        || rand::thread_rng().gen_range(0.0..=1.0) <= save_chance)
                {
                    error_handler
                } else {
                    // TODO: is always logging at debug level fine?
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
                RequestErrorHandler::SaveReverts(_) => {
                    // TODO: logging every one is going to flood the database
                    // TODO: have a percent chance to do this. or maybe a "logged reverts per second"
                    if let ProviderError::JsonRpcClientError(err) = err {
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
                                // spawn saving to the database so we don't slow down the request (or error if no db)
                                let f = self
                                    .authorization
                                    .clone()
                                    .save_revert(method.to_string(), params.clone());

                                tokio::spawn(async move { f.await });
                            } else {
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
