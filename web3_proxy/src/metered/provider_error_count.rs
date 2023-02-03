//! A module providing the `JsonRpcErrorCount` metric.

use std::fmt::Debug;
use ethers::providers::ProviderError;
use metered::metric::{Advice, Enter, OnResult};
use metered::{
    atomic::AtomicInt,
    clear::Clear,
    metric::{Counter, Metric},
};
use serde::Serialize;
use std::ops::Deref;
use tracing::{instrument};

/// A metric counting how many times an expression typed std `Result` as
/// returned an `Err` variant.
///
/// This is a light-weight metric.
///
/// By default, `ErrorCount` uses a lock-free `u64` `Counter`, which makes sense
/// in multithread scenarios. Non-threaded applications can gain performance by
/// using a `std::cell:Cell<u64>` instead.
#[derive(Clone, Default, Debug, Serialize)]
pub struct ProviderErrorCount<C: Counter = AtomicInt<u64>>(pub C);

impl<C: Counter + Debug, T: Debug> Metric<Result<T, ProviderError>> for ProviderErrorCount<C> {}

impl<C: Counter + Debug> Enter for ProviderErrorCount<C> {
    type E = ();
    fn enter(&self) {}
}

impl<C: Counter + Debug, T: Debug> OnResult<Result<T, ProviderError>> for ProviderErrorCount<C> {
    /// Unlike the default ErrorCount, this one does not increment for internal jsonrpc errors
    #[instrument(level = "trace")]
    fn on_result(&self, _: (), r: &Result<T, ProviderError>) -> Advice {
        match r {
            Ok(_) => {}
            Err(ProviderError::JsonRpcClientError(_)) => {}
            Err(_) => {
                self.0.incr();
            }
        }
        Advice::Return
    }
}

impl<C: Counter> Clear for ProviderErrorCount<C> {
    #[instrument(skip_all)]
    fn clear(&self) {
        self.0.clear()
    }
}

impl<C: Counter> Deref for ProviderErrorCount<C> {
    type Target = C;

    #[instrument(skip_all)]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
