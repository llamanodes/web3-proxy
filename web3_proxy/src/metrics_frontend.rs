use axum::headers::HeaderName;
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use axum::{routing::get, Extension, Router};
use log::info;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::app::Web3ProxyApp;

/// Run a prometheus metrics server on the given port.

pub async fn serve(app: Arc<Web3ProxyApp>, port: u16) -> anyhow::Result<()> {
    // build our application with a route
    // order most to least common
    // TODO: 404 any unhandled routes?
    let app = Router::new().route("/", get(root)).layer(Extension(app));

    // run our app with hyper
    // TODO: allow only listening on localhost?
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!("prometheus listening on port {}", port);
    // TODO: into_make_service is enough if we always run behind a proxy. make into_make_service_with_connect_info optional?

    /*
    It sequentially looks for an IP in:
      - x-forwarded-for header (de-facto standard)
      - x-real-ip header
      - forwarded header (new standard)
      - axum::extract::ConnectInfo (if not behind proxy)

    So we probably won't need into_make_service_with_connect_info, but it shouldn't hurt
    */
    let service = app.into_make_service_with_connect_info::<SocketAddr>();
    // let service = app.into_make_service();

    // `axum::Server` is a re-export of `hyper::Server`
    axum::Server::bind(&addr)
        // TODO: option to use with_connect_info. we want it in dev, but not when running behind a proxy, but not
        .serve(service)
        .await
        .map_err(Into::into)
}

async fn root(Extension(app): Extension<Arc<Web3ProxyApp>>) -> Response {
    let serialized = app.prometheus_metrics();

    let mut r = serialized.into_response();

    // // TODO: is there an easier way to do this?
    r.headers_mut().insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/openmetrics-text; version=1.0.0; charset=utf-8"),
    );

    r
}
