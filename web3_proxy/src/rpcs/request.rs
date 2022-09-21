use super::connection::Web3Connection;
use super::provider::Web3Provider;
use crate::metered::{JsonRpcErrorCount, ProviderErrorCount};
use ethers::providers::{HttpClientError, ProviderError, WsClientError};
use metered::metered;
use metered::HitCount;
use metered::ResponseTime;
use metered::Throughput;
use parking_lot::Mutex;
use std::fmt;
use std::sync::atomic;
use std::sync::Arc;
use tokio::time::{sleep, Duration, Instant};
use tracing::Level;
use tracing::{debug, error, trace, warn, Event};

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
    conn: Mutex<Option<Arc<Web3Connection>>>,
    // TODO: this is the same metrics on the conn. use a reference?
    metrics: Arc<OpenRequestHandleMetrics>,
}

pub enum RequestErrorHandler {
    SaveReverts(f32),
    DebugLevel,
    ErrorLevel,
    WarnLevel,
}

impl From<Level> for RequestErrorHandler {
    fn from(level: Level) -> Self {
        match level {
            Level::DEBUG => RequestErrorHandler::DebugLevel,
            Level::ERROR => RequestErrorHandler::ErrorLevel,
            Level::WARN => RequestErrorHandler::WarnLevel,
            _ => unimplemented!(),
        }
    }
}

#[metered(registry = OpenRequestHandleMetrics, visibility = pub)]
impl OpenRequestHandle {
    pub fn new(conn: Arc<Web3Connection>) -> Self {
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

        let conn = Mutex::new(Some(conn));

        Self { conn, metrics }
    }

    pub fn clone_connection(&self) -> Arc<Web3Connection> {
        if let Some(conn) = self.conn.lock().as_ref() {
            conn.clone()
        } else {
            unimplemented!("this shouldn't happen")
        }
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
        T: fmt::Debug + serde::Serialize + Send + Sync,
        R: serde::Serialize + serde::de::DeserializeOwned + fmt::Debug,
    {
        let conn = self
            .conn
            .lock()
            .take()
            .expect("cannot use request multiple times");

        // TODO: use tracing spans properly
        // TODO: requests from customers have request ids, but we should add
        // TODO: including params in this is way too verbose
        trace!(rpc=%conn, %method, "request");

        let mut provider = None;

        while provider.is_none() {
            match conn.provider.read().await.as_ref() {
                None => {
                    warn!(rpc=%conn, "no provider!");
                    // TODO: how should this work? a reconnect should be in progress. but maybe force one now?
                    // TODO: maybe use a watch handle?
                    // TODO: sleep how long? subscribe to something instead?
                    // TODO: this is going to be very verbose!
                    sleep(Duration::from_millis(100)).await
                }
                Some(found_provider) => provider = Some(found_provider.clone()),
            }
        }

        let provider = &*provider.expect("provider was checked already");

        let response = match provider {
            Web3Provider::Http(provider) => provider.request(method, params).await,
            Web3Provider::Ws(provider) => provider.request(method, params).await,
        };

        conn.active_requests.fetch_sub(1, atomic::Ordering::AcqRel);

        if let Err(err) = &response {
            match error_handler {
                RequestErrorHandler::ErrorLevel => {
                    error!(?err, %method, rpc=%conn, "bad response!");
                }
                RequestErrorHandler::DebugLevel => {
                    debug!(?err, %method, rpc=%conn, "bad response!");
                }
                RequestErrorHandler::WarnLevel => {
                    warn!(?err, %method, rpc=%conn, "bad response!");
                }
                RequestErrorHandler::SaveReverts(chance) => {
                    // TODO: only set SaveReverts if this is an eth_call or eth_estimateGas? we'll need eth_sendRawTransaction somewhere else

                    if let Some(metadata) = tracing::Span::current().metadata() {
                        let fields = metadata.fields();

                        if let Some(user_id) = fields.field("user_id") {
                            let values = [(&user_id, None)];

                            let valueset = fields.value_set(&values);

                            let visitor = todo!();

                            valueset.record(visitor);

                            // TODO: now how we do we get the current value out of it? we might need this index
                        } else {
                            warn!("no user id");
                        }
                    }

                    // TODO: check the span for user_key_id

                    // TODO: only set SaveReverts for
                    // TODO: logging every one is going to flood the database
                    // TODO: have a percent chance to do this. or maybe a "logged reverts per second"
                    if let ProviderError::JsonRpcClientError(err) = err {
                        match provider {
                            Web3Provider::Http(_) => {
                                if let Some(HttpClientError::JsonRpcError(err)) =
                                    err.downcast_ref::<HttpClientError>()
                                {
                                    if err.message.starts_with("execution reverted") {
                                        debug!(%method, ?params, "TODO: save the request");
                                        // TODO: don't do this on the hot path. spawn it
                                    } else {
                                        debug!(?err, %method, rpc=%conn, "bad response!");
                                    }
                                }
                            }
                            Web3Provider::Ws(_) => {
                                if let Some(WsClientError::JsonRpcError(err)) =
                                    err.downcast_ref::<WsClientError>()
                                {
                                    if err.message.starts_with("execution reverted") {
                                        debug!(%method, ?params, "TODO: save the request");
                                        // TODO: don't do this on the hot path. spawn it
                                    } else {
                                        debug!(?err, %method, rpc=%conn, "bad response!");
                                    }
                                }
                            }
                        }
                    }
                }
            }
        } else {
            // TODO: i think ethers already has trace logging (and does it much more fancy)
            // TODO: opt-in response inspection to log reverts with their request. put into redis or what?
            // trace!(rpc=%self.0, %method, ?response);
            trace!(%method, rpc=%conn, "response");
        }

        response
    }
}

impl Drop for OpenRequestHandle {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.lock().take() {
            conn.active_requests.fetch_sub(1, atomic::Ordering::AcqRel);
        }
    }
}
