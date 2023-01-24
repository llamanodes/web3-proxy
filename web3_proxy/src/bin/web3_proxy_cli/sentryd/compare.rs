use anyhow::{anyhow, Context};
use chrono::{DateTime, Utc};
use ethers::types::{Block, TxHash, H256};
use futures::{stream::FuturesUnordered, StreamExt};
use log::{debug, warn};
use serde::{Deserialize, Serialize};
use serde_json::json;
use web3_proxy::jsonrpc::JsonRpcErrorData;

#[derive(Debug, Deserialize, Serialize)]
struct JsonRpcResponse<V> {
    // pub jsonrpc: String,
    // pub id: Box<RawValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<V>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcErrorData>,
}

#[derive(Serialize, Ord, PartialEq, PartialOrd, Eq)]
struct AbbreviatedBlock {
    pub num: u64,
    pub time: DateTime<Utc>,
    pub hash: H256,
}

impl From<Block<TxHash>> for AbbreviatedBlock {
    fn from(x: Block<TxHash>) -> Self {
        Self {
            num: x.number.unwrap().as_u64(),
            hash: x.hash.unwrap(),
            time: x.time().unwrap(),
        }
    }
}

pub async fn main(
    rpc: String,
    others: Vec<String>,
    max_age: i64,
    max_lag: i64,
) -> anyhow::Result<()> {
    let client = reqwest::Client::new();

    let block_by_number_request = json!({
        "jsonrpc": "2.0",
        "id": "1",
        "method": "eth_getBlockByNumber",
        "params": ["latest", false],
    });

    let a = client
        .post(&rpc)
        .json(&block_by_number_request)
        .send()
        .await?
        .json::<JsonRpcResponse<Block<TxHash>>>()
        .await?
        .result
        .unwrap();

    // check the parent because b and c might not be as fast as a
    let parent_hash = a.parent_hash;

    let rpc_block = check_rpc(parent_hash, client.clone(), rpc.clone())
        .await
        .context("Error while querying primary rpc")?;

    let fs = FuturesUnordered::new();
    for other in others.iter() {
        let f = check_rpc(parent_hash, client.clone(), other.clone());

        fs.push(tokio::spawn(f));
    }
    let other_check: Vec<_> = fs.collect().await;

    if other_check.is_empty() {
        return Err(anyhow::anyhow!("No other RPCs to check!"));
    }

    // TODO: collect into a counter instead?
    let mut newest_other = None;
    for oc in other_check.iter() {
        match oc {
            Ok(Ok(x)) => newest_other = newest_other.max(Some(x)),
            Ok(Err(err)) => warn!("failed checking other rpc: {:?}", err),
            Err(err) => warn!("internal error checking other rpc: {:?}", err),
        }
    }

    if let Some(newest_other) = newest_other {
        let duration_since = newest_other
            .time
            .signed_duration_since(rpc_block.time)
            .num_seconds();

        match duration_since.abs().cmp(&max_lag) {
            std::cmp::Ordering::Less | std::cmp::Ordering::Equal => {}
            std::cmp::Ordering::Greater => match duration_since.cmp(&0) {
                std::cmp::Ordering::Equal => unimplemented!(),
                std::cmp::Ordering::Less => {
                    return Err(anyhow::anyhow!(
                        "Our RPC is too far ahead ({} s)! Something might be wrong.\n{:#}\nvs\n{:#}",
                        duration_since.abs(),
                        json!(rpc_block),
                        json!(newest_other),
                    ));
                }
                std::cmp::Ordering::Greater => {
                    return Err(anyhow::anyhow!(
                        "Our RPC is too far behind ({} s)!\n{:#}\nvs\n{:#}",
                        duration_since,
                        json!(rpc_block),
                        json!(newest_other),
                    ));
                }
            },
        }

        let now = Utc::now();

        let block_age = now
            .signed_duration_since(newest_other.max(&rpc_block).time)
            .num_seconds();

        match block_age.abs().cmp(&max_age) {
            std::cmp::Ordering::Less | std::cmp::Ordering::Equal => {}
            std::cmp::Ordering::Greater => match duration_since.cmp(&0) {
                std::cmp::Ordering::Equal => unimplemented!(),
                std::cmp::Ordering::Less => {
                    return Err(anyhow::anyhow!(
                        "Our clock is too far behind ({} s)! Something might be wrong.\n{:#}\nvs\n{:#}",
                        block_age.abs(),
                        json!(now),
                        json!(newest_other),
                    ));
                }
                std::cmp::Ordering::Greater => {
                    return Err(anyhow::anyhow!(
                        "block is too old ({} s)!\n{:#}\nvs\n{:#}",
                        block_age,
                        json!(now),
                        json!(newest_other),
                    ));
                }
            },
        }
    } else {
        return Err(anyhow::anyhow!("No other RPC times to check!"));
    }

    debug!("rpc comparison ok: {:#}", json!(rpc_block));

    Ok(())
}

// i don't think we need a whole provider. a simple http request is easiest
async fn check_rpc(
    block_hash: H256,
    client: reqwest::Client,
    rpc: String,
) -> anyhow::Result<AbbreviatedBlock> {
    let block_by_hash_request = json!({
        "jsonrpc": "2.0",
        "id": "1",
        "method": "eth_getBlockByHash",
        "params": [block_hash, false],
    });

    // TODO: don't unwrap! don't use the try operator
    let response: JsonRpcResponse<Block<TxHash>> = client
        .post(rpc.clone())
        .json(&block_by_hash_request)
        .send()
        .await
        .context(format!("awaiting response from {}", rpc))?
        .json()
        .await
        .context(format!("reading json on {}", rpc))?;

    if let Some(result) = response.result {
        let abbreviated = AbbreviatedBlock::from(result);

        Ok(abbreviated)
    } else if let Some(result) = response.error {
        Err(anyhow!(
            "Failed parsing response from {} as JSON: {:?}",
            rpc,
            result
        ))
    } else {
        unimplemented!("{:?}", response)
    }
}
