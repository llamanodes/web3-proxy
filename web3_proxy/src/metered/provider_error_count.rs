//! A module providing the `JsonRpcErrorCount` metric.

use ethers::providers::ProviderError;
use serde::Serialize;
use std::ops::Deref;

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

impl<C: Counter, T> Metric<Result<T, ProviderError>> for ProviderErrorCount<C> {}

impl<C: Counter> Enter for ProviderErrorCount<C> {
    type E = ();
    fn enter(&self) {}
}

impl<C: Counter, T> OnResult<Result<T, ProviderError>> for ProviderErrorCount<C> {
    /// Unlike the default ErrorCount, this one does not increment for internal jsonrpc errors
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
    fn clear(&self) {
        self.0.clear()
    }
}

impl<C: Counter> Deref for ProviderErrorCount<C> {
    type Target = C;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
