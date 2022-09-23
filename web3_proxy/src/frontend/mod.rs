pub mod authorization;
mod errors;
mod http;
mod rpc_proxy_http;
mod rpc_proxy_ws;
mod users;

use crate::app::Web3ProxyApp;
use ::http::Request;
use axum::{
    body::Body,
    handler::Handler,
    routing::{get, post},
    Extension, Router,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tower_cookies::CookieManagerLayer;
use tower_http::trace::TraceLayer;
use tower_request_id::{RequestId, RequestIdLayer};
use tracing::{error_span, info};

/// http and websocket frontend for customers
pub async fn serve(port: u16, proxy_app: Arc<Web3ProxyApp>) -> anyhow::Result<()> {
    // create a tracing span for each request with a random request id and the method
    // GET: websocket or static pages
    // POST: http rpc or login
    let request_tracing_layer =
        TraceLayer::new_for_http().make_span_with(|request: &Request<Body>| {
            // We get the request id from the extensions
            let request_id = request
                .extensions()
                .get::<RequestId>()
                .map(ToString::to_string)
                .unwrap_or_else(|| "unknown".into());
            // And then we put it along with other information into the `request` span
            error_span!(
                "http_request",
                id = %request_id,
                // TODO: do we want these?
                method = %request.method(),
                // uri = %request.uri(),
            )
        });

    // build our axum Router
    // TODO: these should probbably all start with /rpc. then / can be the static site
    let app = Router::new()
        // routes should be order most to least common
        .route("/rpc", post(rpc_proxy_http::public_proxy_web3_rpc))
        .route("/rpc", get(rpc_proxy_ws::public_websocket_handler))
        .route("/rpc/:user_key", post(rpc_proxy_http::user_proxy_web3_rpc))
        .route("/rpc/:user_key", get(rpc_proxy_ws::user_websocket_handler))
        .route("/rpc/health", get(http::health))
        .route("/rpc/status", get(http::status))
        // TODO: make this optional or remove it since it is available on another port
        .route("/rpc/prometheus", get(http::prometheus))
        .route("/rpc/user/login/:user_address", get(users::get_login))
        .route(
            "/rpc/user/login/:user_address/:message_eip",
            get(users::get_login),
        )
        .route("/rpc/user/login", post(users::post_login))
        .route("/rpc/user", post(users::post_user))
        .route("/rpc/user/logout", get(users::get_logout))
        // layers are ordered bottom up
        // the last layer is first for requests and last for responses
        .layer(Extension(proxy_app))
        // add the request id to our tracing logs
        .layer(request_tracing_layer)
        // create a unique id for each request
        .layer(RequestIdLayer)
        // signed cookies
        .layer(CookieManagerLayer::new())
        // 404 for any unknown routes
        .fallback(errors::handler_404.into_service());

    // run our app with hyper
    // TODO: allow only listening on localhost? top_config.app.host.parse()?
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!("listening on port {}", port);

    // TODO: into_make_service is enough if we always run behind a proxy. make into_make_service_with_connect_info optional?
    /*
    It sequentially looks for an IP in:
      - x-forwarded-for header (de-facto standard)
      - x-real-ip header
      - forwarded header (new standard)
      - axum::extract::ConnectInfo (if not behind proxy)
    */
    let service = app.into_make_service_with_connect_info::<SocketAddr>();
    // let service = app.into_make_service();

    // `axum::Server` is a re-export of `hyper::Server`
    axum::Server::bind(&addr)
        // TODO: option to use with_connect_info. we want it in dev, but not when running behind a proxy, but not
        .serve(service)
        .with_graceful_shutdown(signal_shutdown())
        .await
        .map_err(Into::into)
}

/// Tokio signal handler that will wait for a user to press CTRL+C.
/// We use this in our hyper `Server` method `with_graceful_shutdown`.
async fn signal_shutdown() {
    info!("ctrl-c to quit");
    tokio::signal::ctrl_c()
        .await
        .expect("expect tokio signal ctrl-c");
    info!("signal shutdown");
}
