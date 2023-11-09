use futures::future::BoxFuture;
use futures::FutureExt;
use std::sync::Arc;
use std::time::Duration;
use tokio_retry::strategy::{jitter, FibonacciBackoff};
use tower::retry::budget::Budget;
use tower::retry::Policy;

#[derive(Clone)]
pub struct RetryPolicy {
    budget: Arc<Budget>,
    backoff: FibonacciBackoff,
}

impl RetryPolicy {
    pub fn new(budget: Budget, first_delay_millis: u64, max_delay: Duration) -> Self {
        let retry_strategy = FibonacciBackoff::from_millis(first_delay_millis).max_delay(max_delay);

        Self {
            budget: Arc::new(budget),
            backoff: retry_strategy,
        }
    }
}

impl<T: Clone + 'static, Res, E> Policy<T, Res, E> for RetryPolicy {
    type Future = BoxFuture<'static, Self>;

    fn retry(&self, _: &T, result: Result<&Res, &E>) -> Option<Self::Future> {
        match result {
            Ok(_) => {
                self.budget.deposit();

                None
            }
            Err(_) => match self.budget.withdraw() {
                Ok(_) => Some({
                    let mut pol = self.clone();

                    async move {
                        let millis = pol.backoff.by_ref().map(jitter).next().unwrap();

                        tokio::time::sleep(millis).await;

                        pol
                    }
                    .boxed()
                }),
                Err(_) => None,
            },
        }
    }

    fn clone_request(&self, req: &T) -> Option<T> {
        Some(req.clone())
    }
}
