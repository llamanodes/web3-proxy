use super::connection::Web3Connection;
use super::provider::Web3Provider;
use crate::app::{flatten_handle, AnyhowJoinHandle};
use crate::config::BlockAndRpc;
use anyhow::Context;
use ethers::prelude::{Block, Bytes, Middleware, ProviderError, TxHash, H256, U64};
use futures::future::try_join_all;
use futures::StreamExt;
use parking_lot::RwLock;
use redis_rate_limit::{RedisPool, RedisRateLimit, ThrottleResult};
use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{self, AtomicU32, AtomicU64};
use std::{cmp::Ordering, sync::Arc};
use tokio::sync::broadcast;
use tokio::sync::RwLock as AsyncRwLock;
use tokio::time::{interval, sleep, sleep_until, Duration, Instant, MissedTickBehavior};
use tracing::{error, info, info_span, instrument, trace, warn, Instrument};

// TODO: rename this
pub enum RequestHandleResult {
    ActiveRequest(PendingRequestHandle),
    RetryAt(Instant),
    None,
}

/// Drop this once a connection completes
pub struct PendingRequestHandle(Arc<Web3Connection>);

impl PendingRequestHandle {
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

impl Drop for PendingRequestHandle {
    fn drop(&mut self) {
        self.0
            .active_requests
            .fetch_sub(1, atomic::Ordering::AcqRel);
    }
}
