//! `frontend` contains HTTP and websocket endpoints for use by a website or web3 wallet.
//!
//! Important reading about axum extractors: <https://docs.rs/axum/latest/axum/extract/index.html#the-order-of-extractors>

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
    routing::{get, post},
    Extension, Router,
};
use http::header::AUTHORIZATION;
use listenfd::ListenFd;
use log::info;
use moka::future::Cache;
use std::net::SocketAddr;
use std::sync::Arc;
use std::{iter::once, time::Duration};
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;
use tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer;

/// simple keys for caching responses
#[derive(Clone, Hash, PartialEq, Eq)]
pub enum FrontendResponseCaches {
    Status,
}

pub type FrontendJsonResponseCache =
    Cache<FrontendResponseCaches, Arc<serde_json::Value>, hashbrown::hash_map::DefaultHashBuilder>;
pub type FrontendHealthCache = Cache<(), bool, hashbrown::hash_map::DefaultHashBuilder>;

/// Start the frontend server.
pub async fn serve(
    port: u16,
    proxy_app: Arc<Web3ProxyApp>,
    mut shutdown_receiver: broadcast::Receiver<()>,
    shutdown_complete_sender: broadcast::Sender<()>,
) -> anyhow::Result<()> {
    // setup caches for whatever the frontend needs
    // no need for max items since it is limited by the enum key
    let json_response_cache: FrontendJsonResponseCache = Cache::builder()
        .time_to_live(Duration::from_secs(2))
        .build_with_hasher(hashbrown::hash_map::DefaultHashBuilder::default());

    // /health gets a cache with a shorter lifetime
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
        // Websocket RPC (GET)
        // If not an RPC, GET will redirect to urls in the config
        //
        // public
        .route(
            "/",
            post(rpc_proxy_http::proxy_web3_rpc).get(rpc_proxy_ws::websocket_handler),
        )
        // authenticated with and without trailing slash
        .route(
            "/rpc/:rpc_key/",
            post(rpc_proxy_http::proxy_web3_rpc_with_key)
                .get(rpc_proxy_ws::websocket_handler_with_key),
        )
        .route(
            "/rpc/:rpc_key",
            post(rpc_proxy_http::proxy_web3_rpc_with_key)
                .get(rpc_proxy_ws::websocket_handler_with_key),
        )
        // authenticated debug route with and without trailing slash
        .route(
            "/debug/:rpc_key/",
            post(rpc_proxy_http::debug_proxy_web3_rpc_with_key)
                .get(rpc_proxy_ws::debug_websocket_handler_with_key),
        )
        .route(
            "/debug/:rpc_key",
            post(rpc_proxy_http::debug_proxy_web3_rpc_with_key)
                .get(rpc_proxy_ws::debug_websocket_handler_with_key),
        )
        // public fastest with and without trailing slash
        .route(
            "/fastest/",
            post(rpc_proxy_http::fastest_proxy_web3_rpc)
                .get(rpc_proxy_ws::fastest_websocket_handler),
        )
        .route(
            "/fastest",
            post(rpc_proxy_http::fastest_proxy_web3_rpc)
                .get(rpc_proxy_ws::fastest_websocket_handler),
        )
        // authenticated fastest with and without trailing slash
        .route(
            "/fastest/:rpc_key/",
            post(rpc_proxy_http::fastest_proxy_web3_rpc_with_key)
                .get(rpc_proxy_ws::fastest_websocket_handler_with_key),
        )
        .route(
            "/fastest/:rpc_key",
            post(rpc_proxy_http::fastest_proxy_web3_rpc_with_key)
                .get(rpc_proxy_ws::fastest_websocket_handler_with_key),
        )
        // public versus
        .route(
            "/versus/",
            post(rpc_proxy_http::versus_proxy_web3_rpc).get(rpc_proxy_ws::versus_websocket_handler),
        )
        .route(
            "/versus",
            post(rpc_proxy_http::versus_proxy_web3_rpc).get(rpc_proxy_ws::versus_websocket_handler),
        )
        // authenticated versus with and without trailing slash
        .route(
            "/versus/:rpc_key/",
            post(rpc_proxy_http::versus_proxy_web3_rpc_with_key)
                .get(rpc_proxy_ws::versus_websocket_handler_with_key),
        )
        .route(
            "/versus/:rpc_key",
            post(rpc_proxy_http::versus_proxy_web3_rpc_with_key)
                .get(rpc_proxy_ws::versus_websocket_handler_with_key),
        )
        //
        // System things
        //
        .route("/health", get(status::health))
        .route("/status", get(status::status))
        .route("/status/backups_needed", get(status::backups_needed))
        //
        // User stuff
        //
        .route("/user/login/:user_address", get(users::user_login_get))
        .route(
            "/user/login/:user_address/:message_eip",
            get(users::user_login_get),
        )
        .route("/user/login", post(users::user_login_post))
        .route("/user", get(users::user_get).post(users::user_post))
        .route("/user/balance", get(users::user_balance_get))
        .route("/user/balance/:txid", post(users::user_balance_post))
        .route(
            "/user/keys",
            get(users::rpc_keys_get)
                .post(users::rpc_keys_management)
                .put(users::rpc_keys_management),
        )
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
        .layer(Extension(proxy_app))
        // frontend caches
        .layer(Extension(json_response_cache))
        .layer(Extension(health_cache))
        // 404 for any unknown routes
        .fallback(errors::handler_404);

    // TODO: into_make_service is enough if we always run behind a proxy. make into_make_service_with_connect_info optional?
    /*
    It sequentially looks for an IP in:
      - x-forwarded-for header (de-facto standard)
      - x-real-ip header
      - forwarded header (new standard)
      - axum::extract::ConnectInfo (if not behind proxy)
    */
    let service = app.into_make_service_with_connect_info::<SocketAddr>();

    let server_builder = if let Some(listener) = ListenFd::from_env().take_tcp_listener(0)? {
        // use systemd socket magic for no downtime deploys
        let addr = listener.local_addr()?;

        info!("listening with fd at {}", addr);

        axum::Server::from_tcp(listener)?
    } else {
        info!("listening on port {}", port);
        // TODO: allow only listening on localhost? top_config.app.host.parse()?
        let addr = SocketAddr::from(([0, 0, 0, 0], port));

        axum::Server::try_bind(&addr)?
    };

    let server = server_builder
        .serve(service)
        // TODO: option to use with_connect_info. we want it in dev, but not when running behind a proxy, but not
        .with_graceful_shutdown(async move {
            let _ = shutdown_receiver.recv().await;
        })
        .await
        .map_err(Into::into);

    let _ = shutdown_complete_sender.send(());

    server
}
