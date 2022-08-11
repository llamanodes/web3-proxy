/// this should move into web3_proxy once the basics are working
mod errors;
mod http;
mod http_proxy;
mod rate_limit;
mod users;
mod ws_proxy;

use axum::{
    handler::Handler,
    routing::{get, post},
    Extension, Router,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::info;

use crate::app::Web3ProxyApp;

///
pub async fn serve(port: u16, proxy_app: Arc<Web3ProxyApp>) -> anyhow::Result<()> {
    // build our application with a route
    // order most to least common
    let app = Router::new()
        .route("/", post(http_proxy::public_proxy_web3_rpc))
        .route("/", get(ws_proxy::public_websocket_handler))
        .route("/u/:user_key", post(http_proxy::user_proxy_web3_rpc))
        .route("/u/:user_key", get(ws_proxy::user_websocket_handler))
        .route("/health", get(http::health))
        .route("/status", get(http::status))
        .route("/users", post(users::create_user))
        .layer(Extension(proxy_app))
        // 404 for any unknown routes
        .fallback(errors::handler_404.into_service());

    // run our app with hyper
    // `axum::Server` is a re-export of `hyper::Server`
    // TODO: allow only listening on localhost?
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

    axum::Server::bind(&addr)
        // TODO: option to use with_connect_info. we want it in dev, but not when running behind a proxy, but not
        .serve(service)
        .with_graceful_shutdown(async { signal_shutdown().await })
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
