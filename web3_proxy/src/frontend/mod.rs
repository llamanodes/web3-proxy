//! `frontend` contains HTTP and websocket endpoints for use by users and admins.

pub mod authorization;
pub mod errors;
// TODO: these are only public so docs are generated. What's a better way to do this?
pub mod rpc_proxy_http;
pub mod rpc_proxy_ws;
pub mod status;
pub mod users;

use crate::app::Web3ProxyApp;
use axum::{
    routing::{get, post, put},
    Extension, Router,
};
use http::header::AUTHORIZATION;
use log::info;
use moka::future::Cache;
use std::net::SocketAddr;
use std::sync::Arc;
use std::{iter::once, time::Duration};
use tower_http::cors::CorsLayer;
use tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer;

#[derive(Clone, Hash, PartialEq, Eq)]
pub enum FrontendResponseCaches {
    Status,
}

// TODO: what should this cache's value be?
pub type FrontendResponseCache =
    Cache<FrontendResponseCaches, Arc<serde_json::Value>, hashbrown::hash_map::DefaultHashBuilder>;

/// Start the frontend server.
pub async fn serve(port: u16, proxy_app: Arc<Web3ProxyApp>) -> anyhow::Result<()> {
    // setup caches for whatever the frontend needs
    // TODO: a moka cache is probably way overkill for this.
    // no need for max items. only expire because of time to live
    let response_cache: FrontendResponseCache = Cache::builder()
        .time_to_live(Duration::from_secs(1))
        .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default());

    // build our axum Router
    let app = Router::new()
        // routes should be ordered most to least common
        .route("/", post(rpc_proxy_http::proxy_web3_rpc))
        .route("/", get(rpc_proxy_ws::websocket_handler))
        .route(
            "/rpc/:rpc_key",
            post(rpc_proxy_http::proxy_web3_rpc_with_key),
        )
        .route(
            "/rpc/:rpc_key/",
            post(rpc_proxy_http::proxy_web3_rpc_with_key),
        )
        .route(
            "/rpc/:rpc_key",
            get(rpc_proxy_ws::websocket_handler_with_key),
        )
        .route(
            "/rpc/:rpc_key/",
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
        .route("/user/keys", get(users::rpc_keys_get))
        .route("/user/keys", post(users::rpc_keys_management))
        .route("/user/keys", put(users::rpc_keys_management))
        .route("/user/revert_logs", get(users::user_revert_logs_get))
        .route(
            "/user/stats/aggregate",
            get(users::user_stats_aggregated_get),
        )
        .route(
            "/user/stats/aggregated",
            get(users::user_stats_aggregated_get),
        )
        .route("/user/stats/detailed", get(users::user_stats_detailed_get))
        .route("/user/modify_role", get(users::admin_change_user_roles))
        .route("/user/logout", post(users::user_logout_post))
        .route("/status", get(status::status))
        // layers are ordered bottom up
        // the last layer is first for requests and last for responses
        // Mark the `Authorization` request header as sensitive so it doesn't show in logs
        .layer(SetSensitiveRequestHeadersLayer::new(once(AUTHORIZATION)))
        // handle cors
        .layer(CorsLayer::very_permissive())
        // application state
        .layer(Extension(proxy_app.clone()))
        // frontend caches
        .layer(Extension(response_cache))
        // 404 for any unknown routes
        .fallback(errors::handler_404);

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
