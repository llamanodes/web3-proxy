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
    routing::{get, post, put},
    Extension, Router,
};
use http::{header::AUTHORIZATION, StatusCode};
use listenfd::ListenFd;
use log::{debug, info};
use quick_cache_ttl::UnitWeighter;
use std::net::SocketAddr;
use std::sync::Arc;
use std::{iter::once, time::Duration};
use strum::{EnumCount, EnumIter};
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;
use tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer;

use self::errors::Web3ProxyResult;

/// simple keys for caching responses
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, EnumCount, EnumIter)]
pub enum ResponseCacheKey {
    BackupsNeeded,
    Health,
    Status,
}

pub type ResponseCache = quick_cache_ttl::CacheWithTTL<
    ResponseCacheKey,
    (StatusCode, &'static str, axum::body::Bytes),
    UnitWeighter,
    quick_cache_ttl::DefaultHashBuilder,
>;

/// Start the frontend server.
pub async fn serve(
    port: u16,
    proxy_app: Arc<Web3ProxyApp>,
    mut shutdown_receiver: broadcast::Receiver<()>,
    shutdown_complete_sender: broadcast::Sender<()>,
) -> Web3ProxyResult<()> {
    // setup caches for whatever the frontend needs
    // no need for max items since it is limited by the enum key
    // TODO: latest moka allows for different ttls for different
    let response_cache_size = ResponseCacheKey::COUNT;

    debug!("response_cache size: {}", response_cache_size);

    let response_cache = ResponseCache::new(
        "response_cache",
        response_cache_size,
        Duration::from_secs(1),
    )
    .await;

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
        .route(
            "/user/login/:user_address",
            get(users::authentication::user_login_get),
        )
        .route(
            "/user/login/:user_address/:message_eip",
            get(users::authentication::user_login_get),
        )
        .route("/user/login", post(users::authentication::user_login_post))
        .route(
            // /:rpc_key/:subuser_address/:new_status/:new_role
            "/user/subuser",
            get(users::subuser::modify_subuser),
        )
        .route("/user/subusers", get(users::subuser::get_subusers))
        .route(
            "/subuser/rpc_keys",
            get(users::subuser::get_keys_as_subuser),
        )
        .route("/user", get(users::user_get))
        .route("/user", post(users::user_post))
        .route("/user/balance", get(users::payment::user_balance_get))
        .route("/user/deposits", get(users::payment::user_deposits_get))
        .route(
            "/user/balance/:tx_hash",
            get(users::payment::user_balance_post),
        )
        .route("/user/keys", get(users::rpc_keys::rpc_keys_get))
        .route("/user/keys", post(users::rpc_keys::rpc_keys_management))
        .route("/user/keys", put(users::rpc_keys::rpc_keys_management))
        // .route("/user/referral/:referral_link", get(users::user_referral_link_get))
        .route(
            "/user/referral",
            get(users::referral::user_referral_link_get),
        )
        .route("/user/revert_logs", get(users::stats::user_revert_logs_get))
        .route(
            "/user/stats/aggregate",
            get(users::stats::user_stats_aggregated_get),
        )
        .route(
            "/user/stats/aggregated",
            get(users::stats::user_stats_aggregated_get),
        )
        .route(
            "/user/stats/detailed",
            get(users::stats::user_stats_detailed_get),
        )
        .route(
            "/user/logout",
            post(users::authentication::user_logout_post),
        )
        .route(
            "/admin/increase_balance",
            get(admin::admin_increase_balance),
        )
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
        .layer(Extension(Arc::new(response_cache)))
        // 404 for any unknown routes
        .fallback(errors::handler_404);

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

    // into_make_service is enough if we always run behind a proxy
    /*
    It sequentially looks for an IP in:
      - x-forwarded-for header (de-facto standard)
      - x-real-ip header
      - forwarded header (new standard)
      - axum::extract::ConnectInfo (if not behind proxy)
    */
    #[cfg(feature = "connectinfo")]
    let make_service = {
        info!("connectinfo feature enabled");
        app.into_make_service_with_connect_info::<SocketAddr>()
    };

    #[cfg(not(feature = "connectinfo"))]
    let make_service = {
        info!("connectinfo feature disabled");
        app.into_make_service()
    };

    let server = server_builder
        .serve(make_service)
        // TODO: option to use with_connect_info. we want it in dev, but not when running behind a proxy, but not
        .with_graceful_shutdown(async move {
            let _ = shutdown_receiver.recv().await;
        })
        .await
        .map_err(Into::into);

    let _ = shutdown_complete_sender.send(());

    server
}
