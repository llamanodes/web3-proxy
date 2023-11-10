use std::task::{Context, Poll};

use http::Request;
use tower_service::Service;
use ulid::Ulid;

/// RequestId from x-amzn-trace-id header or new Ulid
#[derive(Clone, Debug)]
pub struct RequestId(pub String);

/// Middleware layer for adding RequestId as an Extension
#[derive(Clone, Debug)]
pub struct RequestIdLayer;

impl<S> tower_layer::Layer<S> for RequestIdLayer {
    type Service = RequestIdService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestIdService { inner }
    }
}

/// Service used by RequestIdLayer to inject RequestId
#[derive(Clone, Debug)]
pub struct RequestIdService<S> {
    inner: S,
}

impl<ResBody, S> Service<Request<ResBody>> for RequestIdService<S>
where
    S: Service<Request<ResBody>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<ResBody>) -> Self::Future {
        let request_id = req
            .headers()
            .get("x-amzn-trace-id")
            .and_then(|x| x.to_str().ok())
            .map(ToString::to_string)
            .unwrap_or_else(|| Ulid::new().to_string());
        req.extensions_mut().insert(RequestId(request_id));
        self.inner.call(req)
    }
}
