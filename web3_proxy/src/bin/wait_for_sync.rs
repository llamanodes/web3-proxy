// TODO: websockets instead of http

use anyhow::Context;
use argh::FromArgs;
use chrono::Utc;
use ethers::types::{Block, TxHash};
use log::info;
use log::warn;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use tokio::time::sleep;
use tokio::time::Duration;

#[derive(Debug, FromArgs)]
/// Command line interface for admins to interact with web3_proxy
pub struct CliConfig {
    /// the RPC to check
    #[argh(option, default = "\"http://localhost:8545\".to_string()")]
    pub check_url: String,

    /// the RPC to compare to
    #[argh(option, default = "\"https://eth.llamarpc.com\".to_string()")]
    pub compare_url: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // if RUST_LOG isn't set, configure a default
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "wait_for_sync=debug");
    }

    env_logger::init();

    // this probably won't matter for us in docker, but better safe than sorry
    fdlimit::raise_fd_limit();

    let cli_config: CliConfig = argh::from_env();

    let json_request = json!({
        "id": "1",
        "jsonrpc": "2.0",
        "method": "eth_getBlockByNumber",
        "params": [
            "latest",
            false,
        ],
    });

    let client = reqwest::Client::new();

    // TODO: make sure the chain ids match
    // TODO: automatic compare_url based on the chain id

    loop {
        match main_loop(&cli_config, &client, &json_request).await {
            Ok(()) => break,
            Err(err) => {
                warn!("{:?}", err);
                sleep(Duration::from_secs(10)).await;
            }
        }
    }

    Ok(())
}

#[derive(Deserialize)]
struct JsonRpcBlockResult {
    result: Block<TxHash>,
}

async fn main_loop(
    cli_config: &CliConfig,
    client: &Client,
    json_request: &serde_json::Value,
) -> anyhow::Result<()> {
    let check_result = client
        .post(&cli_config.check_url)
        .json(json_request)
        .send()
        .await
        .context("querying check block")?
        .json::<JsonRpcBlockResult>()
        .await
        .context("parsing check block")?;

    let compare_result = client
        .post(&cli_config.compare_url)
        .json(json_request)
        .send()
        .await
        .context("querying compare block")?
        .json::<JsonRpcBlockResult>()
        .await
        .context("parsing compare block")?;

    let check_block = check_result.result;
    let compare_block = compare_result.result;

    let check_number = check_block.number.context("no check block number")?;
    let compare_number = compare_block.number.context("no compare block number")?;

    if check_number < compare_number {
        let diff_number = compare_number - check_number;

        let diff_time = compare_block.timestamp - check_block.timestamp;

        return Err(anyhow::anyhow!(
            "behind by {} blocks ({} < {}). behind by {} seconds",
            diff_number,
            check_number,
            compare_number,
            diff_time,
        ));
    }

    // TODO: check more. like that the hashes are on the same chain

    let check_time = check_block.time().context("parsing check time")?;

    let now = Utc::now();

    let ago = now.signed_duration_since(check_time);

    info!(
        "Synced on block {} @ {} ({} seconds old)",
        check_number,
        check_time.to_rfc3339(),
        ago.num_seconds()
    );

    Ok(())
}
