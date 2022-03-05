use std::sync::Arc;
use warp::Filter;


const ETH_LOCALHOST_RPC: &str = "http://localhost:8545";
const ETH_EDEN_RPC: &str = "https://api.edennetwork.io/v1/beta";


#[derive(argh::FromArgs)]
/// Proxy Web3 Requests
struct Web3ProxyConfig {
    /// the primary Ethereum RPC server
    #[argh(option, default = "ETH_LOCALHOST_RPC.to_string()")]
    eth_primary_rpc: String,

    /// the private Ethereum RPC server
    #[argh(option, default = "ETH_EDEN_RPC.to_string()")]
    eth_private_rpc: String,

    /// the port to listen on
    #[argh(option, default = "8845")]
    listen_port: u16,
}

#[tokio::main]
async fn main() {
    let config: Web3ProxyConfig = argh::from_env();

    let config = Arc::new(config);

    let listen_port = config.listen_port;

    let hello = warp::path::end().map(|| format!("Hello, world!"));

    let proxy_eth_filter = warp::path!("eth")
        .and(warp::post())
        .and(warp::body::json())
        .then(move |json_body| proxy_eth_rpc(config.clone(), json_body))
        .map(handle_anyhow_errors);

    // TODO: relay ftm, bsc, polygon, avax, ...

    let routes = warp::any().and(hello.or(proxy_eth_filter));

    warp::serve(routes).run(([127, 0, 0, 1], listen_port)).await;
}

/// send the request to the approriate Ethereum RPC
async fn proxy_eth_rpc(
    config: Arc<Web3ProxyConfig>,
    json_body: serde_json::Value,
) -> anyhow::Result<impl warp::Reply> {
    // TODO: this should be a list of servers. query all. prefer result from first server even if it is a bit late
    // TODO: automatically rank servers based on request latency and block height
    let eth_send_signed_transaction =
        serde_json::Value::String("eth_sendSignedTransaction".to_string());

    let upstream_server = if json_body.get("method") == Some(&eth_send_signed_transaction) {
        &config.eth_private_rpc
    } else {
        // TODO: if querying a block older than 256 blocks old, send to an archive node. otherwise a fast sync node is fine
        &config.eth_primary_rpc
    };

    // TODO: reuse this Client/RequestBuilder
    let client = reqwest::Client::new();
    let res = client.post(upstream_server).json(&json_body).send().await?;

    Ok(res.text().await?)
}

/// convert result into an http response
pub fn handle_anyhow_errors<T: warp::Reply>(res: anyhow::Result<T>) -> Box<dyn warp::Reply> {
    match res {
        Ok(r) => Box::new(r.into_response()),
        // TODO: json error?
        Err(e) => Box::new(warp::reply::with_status(
            format!("{}", e),
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        )),
    }
}
