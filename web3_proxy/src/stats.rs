use axum::headers::HeaderName;
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use axum::{routing::get, Extension, Router};
use prometheus_client::encoding::text::encode;
use prometheus_client::encoding::text::Encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::registry::Registry;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::info;

#[derive(Clone, Hash, PartialEq, Eq, Encode)]
pub struct ProxyRequestLabels {
    /// GET is websocket, POST is http
    pub protocol: Protocol,
    /// jsonrpc 2.0 method
    pub rpc_method: String,
    /// anonymous is user 0
    pub user_id: u64,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, Encode)]
pub enum Protocol {
    HTTP,
    Websocket,
}

pub struct AppStatsRegistry {
    pub registry: Registry,
    pub stats: AppStats,
}

#[derive(Clone)]
pub struct AppStats {
    pub proxy_requests: Family<ProxyRequestLabels, Counter>,
}

impl AppStatsRegistry {
    pub fn new() -> Arc<Self> {
        // Note the angle brackets to make sure to use the default (dynamic
        // dispatched boxed metric) for the generic type parameter.
        let mut registry = <Registry>::default();

        // stats for GET and POST
        let proxy_requests = Family::<ProxyRequestLabels, Counter>::default();
        registry.register(
            // With the metric name.
            "http_requests",
            // And the metric help text.
            "Number of HTTP requests received",
            Box::new(proxy_requests.clone()),
        );

        let new = Self {
            registry,
            stats: AppStats { proxy_requests },
        };

        Arc::new(new)
    }

    pub async fn serve(self: Arc<Self>, port: u16) -> anyhow::Result<()> {
        // build our application with a route
        // order most to least common
        // TODO: 404 any unhandled routes?
        let app = Router::new()
            .route("/", get(root))
            .layer(Extension(self.clone()));

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
}

async fn root(Extension(stats_registry): Extension<Arc<AppStatsRegistry>>) -> Response {
    let mut buffer = vec![];

    encode(&mut buffer, &stats_registry.registry).unwrap();

    let s = String::from_utf8(buffer).unwrap();

    let mut r = s.into_response();

    // // TODO: is there an easier way to do this?
    r.headers_mut().insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/openmetrics-text; version=1.0.0; charset=utf-8"),
    );

    r
}
