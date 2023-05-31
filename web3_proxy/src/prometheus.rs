use axum::headers::HeaderName;
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use axum::{routing::get, Extension, Router};
use log::info;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::app::Web3ProxyApp;
use crate::errors::Web3ProxyResult;

/// Run a prometheus metrics server on the given port.
pub async fn serve(
    app: Arc<Web3ProxyApp>,
    port: u16,
    mut shutdown_receiver: broadcast::Receiver<()>,
) -> Web3ProxyResult<()> {
    // routes should be ordered most to least common
    let app = Router::new().route("/", get(root)).layer(Extension(app));

    // TODO: config for the host?
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!("prometheus listening on port {}", port);

    let service = app.into_make_service();

    // `axum::Server` is a re-export of `hyper::Server`
    axum::Server::bind(&addr)
        .serve(service)
        .with_graceful_shutdown(async move {
            let _ = shutdown_receiver.recv().await;
        })
        .await
        .map_err(Into::into)
}

async fn root(Extension(app): Extension<Arc<Web3ProxyApp>>) -> Response {
    let serialized = app.prometheus_metrics().await;

    let mut r = serialized.into_response();

    // // TODO: is there an easier way to do this?
    r.headers_mut().insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/openmetrics-text; version=1.0.0; charset=utf-8"),
    );

    r
}
