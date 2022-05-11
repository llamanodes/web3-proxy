mod app;
mod config;
mod connection;
mod connections;
mod jsonrpc;

use std::fs;
use std::sync::Arc;
use tracing::{info, trace, warn};
use warp::Filter;
use warp::Reply;

use crate::app::Web3ProxyApp;
use crate::config::{CliConfig, RpcConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // install global collector configured based on RUST_LOG env var.
    tracing_subscriber::fmt::init();

    info!("test info");
    warn!("test warn");
    trace!("test trace");

    let cli_config: CliConfig = argh::from_env();

    info!("Loading rpc config @ {}", cli_config.rpc_config_path);
    let rpc_config: String = fs::read_to_string(cli_config.rpc_config_path)?;
    let rpc_config: RpcConfig = toml::from_str(&rpc_config)?;

    // TODO: load the config from yaml instead of hard coding
    // TODO: support multiple chains in one process? then we could just point "chain.stytt.com" at this and caddy wouldn't need anything else
    // TODO: be smart about about using archive nodes? have a set that doesn't use archive nodes since queries to them are more valuable
    let listen_port = cli_config.listen_port;

    let app = rpc_config.try_build().await?;

    let app: Arc<Web3ProxyApp> = Arc::new(app);

    let proxy_rpc_filter = warp::any()
        .and(warp::post())
        .and(warp::body::json())
        .then(move |json_body| app.clone().proxy_web3_rpc(json_body));

    // TODO: filter for displaying connections and their block heights

    // TODO: warp trace is super verbose. how do we make this more readable?
    // let routes = proxy_rpc_filter.with(warp::trace::request());
    let routes = proxy_rpc_filter.map(handle_anyhow_errors);

    warp::serve(routes).run(([0, 0, 0, 0], listen_port)).await;

    Ok(())
}

/// convert result into a jsonrpc error. use this at the end of your warp filter
fn handle_anyhow_errors<T: warp::Reply>(
    res: anyhow::Result<T>,
) -> warp::http::Response<warp::hyper::Body> {
    match res {
        Ok(r) => r.into_response(),
        Err(e) => warp::reply::with_status(
            // TODO: json error
            format!("{}", e),
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        )
        .into_response(),
    }
}
