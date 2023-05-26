use anyhow::{anyhow, Context};
use chrono::{DateTime, Utc};
use ethers::types::{Block, TxHash, H256};
use futures::{stream::FuturesUnordered, StreamExt};
use log::{debug, warn};
use serde::{Deserialize, Serialize};
use serde_json::json;
use web3_proxy::jsonrpc::JsonRpcErrorData;

use super::{SentrydErrorBuilder, SentrydResult};

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
    error_builder: SentrydErrorBuilder,
    rpc: String,
    others: Vec<String>,
    max_age: i64,
    max_lag: i64,
) -> SentrydResult {
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
        .await
        .context(format!("error querying block from {}", rpc))
        .map_err(|x| error_builder.build(x))?;

    if !a.status().is_success() {
        return error_builder.result(anyhow!("bad response from {}: {}", rpc, a.status()));
    }

    // TODO: capture response headers now in case of error. store them in the extra data on the pager duty alert
    let headers = format!("{:#?}", a.headers());

    let body = a
        .text()
        .await
        .context(format!("failed parsing body from {}", rpc))
        .map_err(|x| error_builder.build(x))?;

    let a: JsonRpcResponse<Block<TxHash>> = serde_json::from_str(&body)
        .context(format!("body: {}", body))
        .context(format!("failed parsing json from {}", rpc))
        .map_err(|x| error_builder.build(x))?;

    let a = if let Some(block) = a.result {
        block
    } else if let Some(err) = a.error {
        return error_builder.result(
            anyhow::anyhow!("headers: {:#?}. err: {:#?}", headers, err)
                .context(format!("jsonrpc error from {}: code {}", rpc, err.code)),
        );
    } else {
        return error_builder
            .result(anyhow!("{:#?}", a).context(format!("empty response from {}", rpc)));
    };

    // check the parent because b and c might not be as fast as a
    let parent_hash = a.parent_hash;

    let rpc_block = check_rpc(parent_hash, client.clone(), rpc.to_string())
        .await
        .context(format!("Error while querying primary rpc: {}", rpc))
        .map_err(|err| error_builder.build(err))?;

    let fs = FuturesUnordered::new();
    for other in others.iter() {
        let f = check_rpc(parent_hash, client.clone(), other.to_string());

        fs.push(tokio::spawn(f));
    }
    let other_check: Vec<_> = fs.collect().await;

    if other_check.is_empty() {
        return error_builder.result(anyhow::anyhow!("No other RPCs to check!"));
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
                std::cmp::Ordering::Equal => {
                    unimplemented!("we already checked that they are not equal")
                }
                std::cmp::Ordering::Less => {
                    return error_builder.result(anyhow::anyhow!(
                        "Our RPC is too far ahead ({} s)! Something might be wrong.\n{:#}\nvs\n{:#}",
                        duration_since.abs(),
                        json!(rpc_block),
                        json!(newest_other),
                    ).context(format!("{} is too far ahead", rpc)));
                }
                std::cmp::Ordering::Greater => {
                    return error_builder.result(
                        anyhow::anyhow!(
                            "Behind {} s!\n{:#}\nvs\n{:#}",
                            duration_since,
                            json!(rpc_block),
                            json!(newest_other),
                        )
                        .context(format!("{} is too far behind", rpc)),
                    );
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
                    return error_builder.result(
                        anyhow::anyhow!(
                            "Clock is behind {}s! Something might be wrong.\n{:#}\nvs\n{:#}",
                            block_age.abs(),
                            json!(now),
                            json!(newest_other),
                        )
                        .context(format!("Clock is too far behind on {}!", rpc)),
                    );
                }
                std::cmp::Ordering::Greater => {
                    return error_builder.result(
                        anyhow::anyhow!(
                            "block is too old ({}s)!\n{:#}\nvs\n{:#}",
                            block_age,
                            json!(now),
                            json!(newest_other),
                        )
                        .context(format!("block is too old on {}!", rpc)),
                    );
                }
            },
        }
    } else {
        return error_builder.result(anyhow::anyhow!("No other RPC times to check!"));
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

    let response = client
        .post(&rpc)
        .json(&block_by_hash_request)
        .send()
        .await
        .context(format!("awaiting response from {}", rpc))?;

    anyhow::ensure!(
        response.status().is_success(),
        "bad response from {}: {}",
        rpc,
        response.status(),
    );

    let body = response
        .text()
        .await
        .context(format!("failed parsing body from {}", rpc))?;

    let response_json: JsonRpcResponse<Block<TxHash>> = serde_json::from_str(&body)
        .context(format!("body: {}", body))
        .context(format!("failed parsing json from {}", rpc))?;

    if let Some(result) = response_json.result {
        let abbreviated = AbbreviatedBlock::from(result);

        debug!("{} has {:?}@{}", rpc, abbreviated.hash, abbreviated.num);

        Ok(abbreviated)
    } else if let Some(result) = response_json.error {
        Err(anyhow!(
            "jsonrpc error during check_rpc from {}: {:#}",
            rpc,
            json!(result),
        ))
    } else {
        Err(anyhow!(
            "empty result during check_rpc from {}: {:#}",
            rpc,
            json!(response_json)
        ))
    }
}
