// TODO: support websockets

use anyhow::Context;
use argh::FromArgs;
use chrono::Utc;
use ethers::types::U64;
use ethers::types::{Block, TxHash};
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::time::sleep;
use tokio::time::Duration;
use tracing::info;
use tracing::warn;

#[derive(Debug, FromArgs)]
/// Command line interface for admins to interact with web3_proxy
pub struct CliConfig {
    /// the HTTP RPC to check
    #[argh(option, default = "\"http://localhost:8545\".to_string()")]
    pub check_url: String,

    /// the HTTP RPC to compare against. defaults to LlamaNodes public RPC
    #[argh(option)]
    pub compare_url: Option<String>,

    /// how many seconds to wait for sync.
    /// Defaults to waiting forever.
    /// if the wait is exceeded, will exit with code 2
    #[argh(option)]
    pub max_wait: Option<u64>,

    /// require a specific chain id (for extra safety)
    #[argh(option)]
    pub chain_id: Option<u64>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // if RUST_LOG isn't set, configure a default
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "wait_for_sync=debug");
    }

    // todo!("set up tracing");

    // this probably won't matter for us in docker, but better safe than sorry
    fdlimit::raise_fd_limit();

    let cli_config: CliConfig = argh::from_env();

    let client = reqwest::Client::new();

    let check_url = cli_config.check_url;

    // make sure the chain ids match
    let check_id = get_chain_id(&check_url, &client)
        .await
        .context("unknown chain id for check_url")?;

    if let Some(chain_id) = cli_config.chain_id {
        anyhow::ensure!(
            chain_id == check_id,
            "chain_id of check_url is wrong! Need {}. Found {}",
            chain_id,
            check_id,
        );
    }

    let compare_url: String = match cli_config.compare_url {
        Some(x) => x,
        None => match check_id {
            1 => "https://eth.llamarpc.com",
            137 => "https://polygon.llamarpc.com",
            _ => {
                return Err(anyhow::anyhow!(
                    "--compare-url required for chain {}",
                    check_id
                ))
            }
        }
        .to_string(),
    };

    info!(
        "comparing {} to {} (chain {})",
        check_url, compare_url, check_id
    );

    let compare_id = get_chain_id(&compare_url, &client)
        .await
        .context("unknown chain id for compare_url")?;

    anyhow::ensure!(
        check_id == compare_id,
        "chain_id does not match! Need {}. Found {}",
        check_id,
        compare_id,
    );

    // start ids at 2 because id 1 was checking the chain id
    let counter = AtomicU32::new(2);
    let start = tokio::time::Instant::now();

    loop {
        match main_loop(&check_url, &compare_url, &client, &counter).await {
            Ok(()) => break,
            Err(err) => {
                warn!(?err, "main_loop");

                if let Some(max_wait) = cli_config.max_wait {
                    if max_wait == 0 || start.elapsed().as_secs() > max_wait {
                        std::process::exit(2);
                    }
                }

                sleep(Duration::from_secs(10)).await;
            }
        }
    }

    Ok(())
}

#[derive(Deserialize)]
struct JsonRpcChainIdResult {
    result: U64,
}

async fn get_chain_id(rpc: &str, client: &reqwest::Client) -> anyhow::Result<u64> {
    // empty params aren't required by the spec, but some rpc providers require them
    let get_chain_id_request = json!({
        "id": "1",
        "jsonrpc": "2.0",
        "method": "eth_chainId",
        "params": [],
    });

    // TODO: loop until chain id is found?
    let check_result = client
        .post(rpc)
        .json(&get_chain_id_request)
        .send()
        .await
        .context("failed querying chain id")?
        .json::<JsonRpcChainIdResult>()
        .await
        .context("failed parsing chain id")?
        .result
        .as_u64();

    Ok(check_result)
}

#[derive(Deserialize)]
struct JsonRpcBlockResult {
    result: Block<TxHash>,
}

async fn main_loop(
    check_url: &str,
    compare_url: &str,
    client: &Client,
    counter: &AtomicU32,
) -> anyhow::Result<()> {
    // TODO: have a real id here that increments every call?
    let get_block_number_request = json!({
        "id": counter.fetch_add(1, Ordering::SeqCst),
        "jsonrpc": "2.0",
        "method": "eth_getBlockByNumber",
        "params": [
            "latest",
            false,
        ],
    });

    let check_block = client
        .post(check_url)
        .json(&get_block_number_request)
        .send()
        .await
        .context("querying check block")?
        .json::<JsonRpcBlockResult>()
        .await
        .context("parsing check block")?
        .result;

    let compare_block = client
        .post(compare_url)
        .json(&get_block_number_request)
        .send()
        .await
        .context("querying compare block")?
        .json::<JsonRpcBlockResult>()
        .await
        .context("parsing compare block")?
        .result;

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
