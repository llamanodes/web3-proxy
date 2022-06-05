/// this should move into web3-proxy once the basics are working
mod errors;
mod http;
mod http_proxy;
mod ws_proxy;
use axum::{
    handler::Handler,
    routing::{get, post},
    Extension, Router,
};
use std::net::SocketAddr;
use std::sync::Arc;

use crate::app::Web3ProxyApp;

pub async fn run(port: u16, proxy_app: Arc<Web3ProxyApp>) -> anyhow::Result<()> {
    // TODO: check auth (from authp?) here
    // build our application with a route
    let app = Router::new()
        // `POST /` goes to `proxy_web3_rpc`
        .route("/", post(http_proxy::proxy_web3_rpc))
        // `websocket /` goes to `proxy_web3_ws`
        .route("/", get(ws_proxy::websocket_handler))
        // `GET /index.html` goes to `index`
        .route("/index.html", get(http::index))
        // `GET /status` goes to `status`
        .route("/status", get(http::status))
        .layer(Extension(proxy_app));

    // 404 for any unknown routes
    let app = app.fallback(errors::handler_404.into_service());

    // run our app with hyper
    // `axum::Server` is a re-export of `hyper::Server`
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("listening on port {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .map_err(Into::into)
}
