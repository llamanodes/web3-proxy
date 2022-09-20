use super::connection::Web3Connection;
use super::provider::Web3Provider;
use ethers::providers::ProviderError;
use metered::metered;
use metered::ErrorCount;
use metered::HitCount;
use metered::ResponseTime;
use metered::Throughput;
use parking_lot::Mutex;
use std::fmt;
use std::sync::atomic;
use std::sync::Arc;
use tokio::time::{sleep, Duration, Instant};
use tracing::warn;
use tracing::{instrument, trace};

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
    #[instrument(skip_all)]
    #[measure([ErrorCount, HitCount, ResponseTime, Throughput])]
    pub async fn request<T, R>(
        &self,
        method: &str,
        params: T,
        // TODO: change this to error_log_level?
        silent_errors: bool,
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
                    // TODO: sleep how long? subscribe to something instead?
                    sleep(Duration::from_millis(100)).await
                }
                Some(found_provider) => provider = Some(found_provider.clone()),
            }
        }

        let response = match &*provider.expect("provider was checked already") {
            Web3Provider::Http(provider) => provider.request(method, params).await,
            Web3Provider::Ws(provider) => provider.request(method, params).await,
        };

        // no need to do conn.active_requests.fetch_sub because Drop will do that

        // TODO: i think ethers already has trace logging (and does it much more fancy)
        if let Err(err) = &response {
            if !silent_errors {
                // TODO: this isn't always bad. missing trie node while we are checking initial
                warn!(?err, %method, rpc=%conn, "bad response!");
            }
        } else {
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
