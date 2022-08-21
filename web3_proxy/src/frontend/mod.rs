mod axum_ext;
mod errors;
mod http;
mod http_proxy;
mod rate_limit;
mod users;
mod ws_proxy;

use crate::app::Web3ProxyApp;
use ::http::{Request, StatusCode};
use axum::{
    body::Body,
    error_handling::HandleError,
    handler::Handler,
    response::Response,
    routing::{get, post},
    Extension, Router,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use tower_request_id::{RequestId, RequestIdLayer};
use tracing::{error_span, info};

// handle errors by converting them into something that implements
// `IntoResponse`
async fn handle_anyhow_error(err: anyhow::Error) -> (StatusCode, String) {
    // TODO: i dont like this, but lets see if it works. need to moved to the errors module and replace the version that is there
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Something went wrong: {}", err),
    )
}

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
    let app = Router::new()
        // routes should be order most to least common
        .route("/", post(http_proxy::public_proxy_web3_rpc))
        .route("/", get(ws_proxy::public_websocket_handler))
        .route("/u/:user_key", post(http_proxy::user_proxy_web3_rpc))
        .route("/u/:user_key", get(ws_proxy::user_websocket_handler))
        .route("/health", get(http::health))
        // TODO: we probably want to remove /status in favor of the separate prometheus thread
        .route("/status", get(http::status))
        .route("/login/:user_address", get(users::get_login))
        .route("/login/:user_address/:message_eip", get(users::get_login))
        .route("/users", post(users::post_user))
        // layers are ordered bottom up
        // the last layer is first for requests and last for responses
        .layer(Extension(proxy_app))
        // add the request id to our tracing logs
        .layer(request_tracing_layer)
        // create a unique id for each request
        .layer(RequestIdLayer)
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

    So we probably won't need into_make_service_with_connect_info, but it shouldn't hurt
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
