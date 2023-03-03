//! `frontend` contains HTTP and websocket endpoints for use by users and admins.
//!
//! Important reading about axum extractors: https://docs.rs/axum/latest/axum/extract/index.html#the-order-of-extractors

pub mod admin;
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
pub type FrontendHealthCache = Cache<(), bool, hashbrown::hash_map::DefaultHashBuilder>;

/// Start the frontend server.
pub async fn serve(port: u16, proxy_app: Arc<Web3ProxyApp>) -> anyhow::Result<()> {
    // setup caches for whatever the frontend needs
    // TODO: a moka cache is probably way overkill for this.
    // no need for max items. only expire because of time to live
    let response_cache: FrontendResponseCache = Cache::builder()
        .time_to_live(Duration::from_secs(2))
        .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default());

    let health_cache: FrontendHealthCache = Cache::builder()
        .time_to_live(Duration::from_millis(100))
        .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default());

    // TODO: read config for if fastest/versus should be available publicly. default off

    // build our axum Router
    let app = Router::new()
        // TODO: i think these routes could be done a lot better
        //
        // HTTP RPC (POST)
        //
        // public
        .route("/", post(rpc_proxy_http::proxy_web3_rpc))
        // authenticated with and without trailing slash
        .route(
            "/rpc/:rpc_key/",
            post(rpc_proxy_http::proxy_web3_rpc_with_key),
        )
        .route(
            "/rpc/:rpc_key",
            post(rpc_proxy_http::proxy_web3_rpc_with_key),
        )
        // authenticated debug route with and without trailing slash
        .route(
            "/debug/:rpc_key/",
            post(rpc_proxy_http::debug_proxy_web3_rpc_with_key),
        )
        .route(
            "/debug/:rpc_key",
            post(rpc_proxy_http::debug_proxy_web3_rpc_with_key),
        )
        // public fastest with and without trailing slash
        .route("/fastest/", post(rpc_proxy_http::fastest_proxy_web3_rpc))
        .route("/fastest", post(rpc_proxy_http::fastest_proxy_web3_rpc))
        // authenticated fastest with and without trailing slash
        .route(
            "/fastest/:rpc_key/",
            post(rpc_proxy_http::fastest_proxy_web3_rpc_with_key),
        )
        .route(
            "/fastest/:rpc_key",
            post(rpc_proxy_http::fastest_proxy_web3_rpc_with_key),
        )
        // public versus
        .route("/versus/", post(rpc_proxy_http::versus_proxy_web3_rpc))
        .route("/versus", post(rpc_proxy_http::versus_proxy_web3_rpc))
        // authenticated versus with and without trailing slash
        .route(
            "/versus/:rpc_key/",
            post(rpc_proxy_http::versus_proxy_web3_rpc_with_key),
        )
        .route(
            "/versus/:rpc_key",
            post(rpc_proxy_http::versus_proxy_web3_rpc_with_key),
        )
        //
        // Websocket RPC (GET)
        // If not an RPC, this will redirect to configurable urls
        //
        // public
        .route("/", get(rpc_proxy_ws::websocket_handler))
        // authenticated with and without trailing slash
        .route(
            "/rpc/:rpc_key/",
            get(rpc_proxy_ws::websocket_handler_with_key),
        )
        .route(
            "/rpc/:rpc_key",
            get(rpc_proxy_ws::websocket_handler_with_key),
        )
        // debug with and without trailing slash
        .route(
            "/debug/:rpc_key/",
            get(rpc_proxy_ws::websocket_handler_with_key),
        )
        .route(
            "/debug/:rpc_key",
            get(rpc_proxy_ws::websocket_handler_with_key),
        ) // public fastest with and without trailing slash
        .route("/fastest/", get(rpc_proxy_ws::fastest_websocket_handler))
        .route("/fastest", get(rpc_proxy_ws::fastest_websocket_handler))
        // authenticated fastest with and without trailing slash
        .route(
            "/fastest/:rpc_key/",
            get(rpc_proxy_ws::fastest_websocket_handler_with_key),
        )
        .route(
            "/fastest/:rpc_key",
            get(rpc_proxy_ws::fastest_websocket_handler_with_key),
        )
        // public versus
        .route(
            "/versus/",
            get(rpc_proxy_ws::versus_websocket_handler_with_key),
        )
        .route(
            "/versus",
            get(rpc_proxy_ws::versus_websocket_handler_with_key),
        )
        // authenticated versus with and without trailing slash
        .route(
            "/versus/:rpc_key/",
            get(rpc_proxy_ws::versus_websocket_handler_with_key),
        )
        .route(
            "/versus/:rpc_key",
            get(rpc_proxy_ws::versus_websocket_handler_with_key),
        )
        //
        // System things
        //
        .route("/health", get(status::health))
        .route("/status", get(status::status))
        //
        // User stuff
        //
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
        .route("/user/logout", post(users::user_logout_post))
        .route("/admin/modify_role", get(admin::admin_change_user_roles))
        .route(
            "/admin/imitate-login/:admin_address/:user_address",
            get(admin::admin_login_get),
        )
        .route(
            "/admin/imitate-login/:admin_address/:user_address/:message_eip",
            get(admin::admin_login_get),
        )
        .route("/admin/imitate-login", post(admin::admin_login_post))
        .route("/admin/imitate-logout", post(admin::admin_logout_post))
        //
        // Axum layers
        // layers are ordered bottom up
        // the last layer is first for requests and last for responses
        //
        // Mark the `Authorization` request header as sensitive so it doesn't show in logs
        .layer(SetSensitiveRequestHeadersLayer::new(once(AUTHORIZATION)))
        // handle cors
        .layer(CorsLayer::very_permissive())
        // application state
        .layer(Extension(proxy_app.clone()))
        // frontend caches
        .layer(Extension(response_cache))
        .layer(Extension(health_cache))
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

    // `axum::Server` is a re-export of `hyper::Server`
    axum::Server::bind(&addr)
        // TODO: option to use with_connect_info. we want it in dev, but not when running behind a proxy, but not
        .serve(service)
        .await
        .map_err(Into::into)
}
