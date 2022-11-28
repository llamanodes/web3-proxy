use argh::FromArgs;
use ethers::types::{Block, TxHash, H256};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use serde_json::json;
use web3_proxy::jsonrpc::JsonRpcErrorData;

#[derive(FromArgs, PartialEq, Debug, Eq)]
/// Never bring only 2 compasses to sea.
#[argh(subcommand, name = "health_compass")]
pub struct HealthCompassSubCommand {
    #[argh(positional)]
    /// first rpc
    rpc_a: String,

    #[argh(positional)]
    /// second rpc
    rpc_b: String,

    #[argh(positional)]
    /// third rpc
    rpc_c: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct JsonRpcResponse<V> {
    // pub jsonrpc: String,
    // pub id: Box<RawValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<V>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcErrorData>,
}

impl HealthCompassSubCommand {
    pub async fn main(self) -> anyhow::Result<()> {
        let client = reqwest::Client::new();

        let block_by_number_request = json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "eth_getBlockByNumber",
            "params": ["latest", false],
        });

        let a = client
            .post(&self.rpc_a)
            .json(&block_by_number_request)
            .send()
            .await?
            .json::<JsonRpcResponse<Block<TxHash>>>()
            .await?
            .result
            .unwrap();

        // check the parent because b and c might not be as fast as a
        let parent_hash = a.parent_hash;

        let a = check_rpc(&parent_hash, &client, &self.rpc_a).await;
        let b = check_rpc(&parent_hash, &client, &self.rpc_b).await;
        let c = check_rpc(&parent_hash, &client, &self.rpc_c).await;

        match (a, b, c) {
            (Ok(Ok(a)), Ok(Ok(b)), Ok(Ok(c))) => {
                if a != b {
                    error!("A: {:?}\n\nB: {:?}\n\nC: {:?}", a, b, c);
                    return Err(anyhow::anyhow!("difference detected!"));
                }

                if b != c {
                    error!("\nA: {:?}\n\nB: {:?}\n\nC: {:?}", a, b, c);
                    return Err(anyhow::anyhow!("difference detected!"));
                }

                // all three rpcs agree
            }
            (Ok(Ok(a)), Ok(Ok(b)), c) => {
                // not all successes! but still enough to compare
                warn!("C failed: {:?}", c);

                if a != b {
                    error!("\nA: {:?}\n\nB: {:?}", a, b);
                    return Err(anyhow::anyhow!("difference detected!"));
                }
            }
            (Ok(Ok(a)), b, Ok(Ok(c))) => {
                // not all successes! but still enough to compare
                warn!("B failed: {:?}", b);

                if a != c {
                    error!("\nA: {:?}\n\nC: {:?}", a, c);
                    return Err(anyhow::anyhow!("difference detected!"));
                }
            }
            (a, b, c) => {
                // not enough successes
                error!("A: {:?}\n\nB: {:?}\n\nC: {:?}", a, b, c);
                return Err(anyhow::anyhow!("All are failing!"));
            }
        }

        info!("OK");

        Ok(())
    }
}

// i don't think we need a whole provider. a simple http request is easiest
async fn check_rpc(
    block_hash: &H256,
    client: &reqwest::Client,
    rpc: &str,
) -> anyhow::Result<Result<Block<TxHash>, JsonRpcErrorData>> {
    let block_by_hash_request = json!({
        "jsonrpc": "2.0",
        "id": "1",
        "method": "eth_getBlockByHash",
        "params": [block_hash, false],
    });

    // TODO: don't unwrap! don't use the try operator
    let response: JsonRpcResponse<Block<TxHash>> = client
        .post(rpc)
        .json(&block_by_hash_request)
        .send()
        .await?
        .json()
        .await?;

    if let Some(result) = response.result {
        Ok(Ok(result))
    } else if let Some(result) = response.error {
        Ok(Err(result))
    } else {
        unimplemented!("{:?}", response)
    }
}
