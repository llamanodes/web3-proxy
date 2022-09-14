use super::connection::Web3Connection;
use super::provider::Web3Provider;
use metered::metered;
use metered::ErrorCount;
use metered::HitCount;
use metered::InFlight;
use metered::ResponseTime;
use metered::Throughput;
use std::fmt;
use std::sync::atomic;
use std::sync::atomic::AtomicBool;
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
    conn: Arc<Web3Connection>,
    // TODO: this is the same metrics on the conn. use a reference
    metrics: Arc<OpenRequestHandleMetrics>,
    decremented: AtomicBool,
}

#[metered(registry = OpenRequestHandleMetrics, visibility = pub)]
impl OpenRequestHandle {
    pub fn new(conn: Arc<Web3Connection>) -> Self {
        // TODO: attach a unique id to this? customer requests have one, but not internal queries
        // TODO: what ordering?!
        // TODO: should we be using metered, or not? i think not because we want stats for each handle
        // TODO: these should maybe be sent to an influxdb instance?
        conn.active_requests.fetch_add(1, atomic::Ordering::AcqRel);

        // TODO: handle overflows?
        // TODO: what ordering?
        conn.total_requests.fetch_add(1, atomic::Ordering::Relaxed);

        let metrics = conn.open_request_handle_metrics.clone();

        let decremented = false.into();

        Self {
            conn,
            metrics,
            decremented,
        }
    }

    pub fn clone_connection(&self) -> Arc<Web3Connection> {
        self.conn.clone()
    }

    /// Send a web3 request
    /// By having the request method here, we ensure that the rate limiter was called and connection counts were properly incremented
    /// By taking self here, we ensure that this is dropped after the request is complete.
    /// TODO: we no longer take self because metered doesn't like that
    /// TODO: ErrorCount includes too many types of errors, such as transaction reverts
    #[instrument(skip_all)]
    #[measure([ErrorCount, HitCount, InFlight, ResponseTime, Throughput])]
    pub async fn request<T, R>(
        &self,
        method: &str,
        params: T,
        silent_errors: bool,
    ) -> Result<R, ethers::prelude::ProviderError>
    where
        T: fmt::Debug + serde::Serialize + Send + Sync,
        R: serde::Serialize + serde::de::DeserializeOwned + fmt::Debug,
    {
        // TODO: use tracing spans properly
        // TODO: requests from customers have request ids, but we should add
        // TODO: including params in this is way too verbose
        trace!(rpc=%self.conn, %method, "request");

        let mut provider = None;

        while provider.is_none() {
            match self.conn.provider.read().await.as_ref() {
                None => {
                    warn!(rpc=%self.conn, "no provider!");
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

        self.decremented.store(true, atomic::Ordering::Release);
        self.conn
            .active_requests
            .fetch_sub(1, atomic::Ordering::AcqRel);
        // todo: do something to make sure this doesn't get called again? i miss having the function sig have self

        // TODO: i think ethers already has trace logging (and does it much more fancy)
        if let Err(err) = &response {
            if !silent_errors {
                // TODO: this isn't always bad. missing trie node while we are checking initial
                warn!(?err, %method, rpc=%self.conn, "bad response!");
            }
        } else {
            // TODO: opt-in response inspection to log reverts with their request. put into redis or what?
            // trace!(rpc=%self.0, %method, ?response);
            trace!(%method, rpc=%self.conn, "response");
        }

        response
    }
}

impl Drop for OpenRequestHandle {
    fn drop(&mut self) {
        if self.decremented.load(atomic::Ordering::Acquire) {
            // we already decremented from a successful request
            return;
        }

        self.conn
            .active_requests
            .fetch_sub(1, atomic::Ordering::AcqRel);
    }
}
