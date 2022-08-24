use super::connection::Web3Connection;
use super::provider::Web3Provider;
use std::fmt;
use std::sync::atomic;
use std::sync::Arc;
use tokio::time::Instant;
use tracing::{instrument, trace};

// TODO: rename this
pub enum OpenRequestResult {
    ActiveRequest(OpenRequestHandle),
    RetryAt(Instant),
    None,
}

/// Drop this once a connection completes
pub struct OpenRequestHandle(Arc<Web3Connection>);

impl OpenRequestHandle {
    pub fn new(connection: Arc<Web3Connection>) -> Self {
        // TODO: attach a unique id to this?
        // TODO: what ordering?!
        connection
            .active_requests
            .fetch_add(1, atomic::Ordering::AcqRel);

        Self(connection)
    }

    pub fn clone_connection(&self) -> Arc<Web3Connection> {
        self.0.clone()
    }

    /// Send a web3 request
    /// By having the request method here, we ensure that the rate limiter was called and connection counts were properly incremented
    /// By taking self here, we ensure that this is dropped after the request is complete
    #[instrument(skip_all)]
    pub async fn request<T, R>(
        &self,
        method: &str,
        params: T,
    ) -> Result<R, ethers::prelude::ProviderError>
    where
        T: fmt::Debug + serde::Serialize + Send + Sync,
        R: serde::Serialize + serde::de::DeserializeOwned + fmt::Debug,
    {
        // TODO: use tracing spans properly
        // TODO: it would be nice to have the request id on this
        // TODO: including params in this is way too verbose
        trace!("Sending {} to {}", method, self.0);

        let mut provider = None;

        while provider.is_none() {
            // TODO: if no provider, don't unwrap. wait until there is one.
            match self.0.provider.read().await.as_ref() {
                None => {}
                Some(found_provider) => provider = Some(found_provider.clone()),
            }
        }

        let response = match &*provider.unwrap() {
            Web3Provider::Http(provider) => provider.request(method, params).await,
            Web3Provider::Ws(provider) => provider.request(method, params).await,
        };

        // TODO: i think ethers already has trace logging (and does it much more fancy)
        // TODO: at least instrument this with more useful information
        // trace!("Reply from {}: {:?}", self.0, response);
        trace!("Reply from {}", self.0);

        response
    }
}

impl Drop for OpenRequestHandle {
    fn drop(&mut self) {
        self.0
            .active_requests
            .fetch_sub(1, atomic::Ordering::AcqRel);
    }
}
