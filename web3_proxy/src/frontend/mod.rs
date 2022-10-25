//! `frontend` contains HTTP and websocket endpoints for use by users and admins.

pub mod authorization;
mod errors;
// TODO: these are only public so docs are generated. What's a better way to do this?
pub mod rpc_proxy_http;
pub mod rpc_proxy_ws;
pub mod status;
pub mod users;

use crate::app::Web3ProxyApp;
use axum::{
    body::Body,
    handler::Handler,
    routing::{get, post},
    Extension, Router,
};
use http::header::AUTHORIZATION;
use http::Request;
use std::iter::once;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer;
use tower_http::trace::TraceLayer;
use tower_request_id::{RequestId, RequestIdLayer};
use tracing::{error_span, info};

/// Start the frontend server.
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
        // routes should be ordered most to least common
        .route("/", post(rpc_proxy_http::proxy_web3_rpc))
        .route("/", get(rpc_proxy_ws::websocket_handler))
        .route(
            "/rpc/:user_key",
            post(rpc_proxy_http::proxy_web3_rpc_with_key),
        )
        .route(
            "/rpc/:user_key",
            get(rpc_proxy_ws::websocket_handler_with_key),
        )
        .route("/health", get(status::health))
        .route("/user/login/:user_address", get(users::user_login_get))
        .route(
            "/user/login/:user_address/:message_eip",
            get(users::user_login_get),
        )
        .route("/user/login", post(users::user_login_post))
        .route("/user", get(users::user_get))
        .route("/user", post(users::user_post))
        .route("/user/balance", get(users::user_balance_get))
        .route("/user/balance/:txid", post(users::user_balance_post))
        .route("/user/keys", get(users::user_keys_get))
        .route("/user/keys", post(users::user_keys_post))
        .route("/user/revert_logs", get(users::user_revert_logs_get))
        .route(
            "/user/stats/aggregate",
            get(users::user_stats_aggregate_get),
        )
        .route("/user/stats/detailed", get(users::user_stats_detailed_get))
        .route("/user/logout", post(users::user_logout_post))
        .route("/status", get(status::status))
        // TODO: make this optional or remove it since it is available on another port
        .route("/prometheus", get(status::prometheus))
        // layers are ordered bottom up
        // the last layer is first for requests and last for responses
        // Mark the `Authorization` request header as sensitive so it doesn't show in logs
        .layer(SetSensitiveRequestHeadersLayer::new(once(AUTHORIZATION)))
        // add the request id to our tracing logs
        .layer(request_tracing_layer)
        // handle cors
        .layer(CorsLayer::very_permissive())
        // create a unique id for each request
        .layer(RequestIdLayer)
        // application state
        .layer(Extension(proxy_app.clone()))
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
        .await
        .map_err(Into::into)
}
