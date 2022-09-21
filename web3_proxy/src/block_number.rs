//! Helper functions for turning ether's BlockNumber into numbers and updating incoming queries to match.
use ethers::{
    prelude::{BlockNumber, U64},
    types::H256,
};
use tracing::warn;

pub fn block_num_to_u64(block_num: BlockNumber, latest_block: U64) -> (bool, U64) {
    match block_num {
        BlockNumber::Earliest => {
            // modified is false because we want the backend to see "pending"
            (false, U64::zero())
        }
        BlockNumber::Latest => {
            // change "latest" to a number
            // modified is true because we want the backend to see the height and not "latest"
            (true, latest_block)
        }
        BlockNumber::Number(x) => {
            // we already have a number
            (false, x)
        }
        BlockNumber::Pending => {
            // TODO: think more about how to handle Pending
            // modified is false because we want the backend to see "pending"
            (false, latest_block)
        }
    }
}

/// modify params to always have a block number and not "latest"
pub fn clean_block_number(
    params: &mut serde_json::Value,
    block_param_id: usize,
    latest_block: U64,
) -> anyhow::Result<U64> {
    match params.as_array_mut() {
        None => {
            // TODO: this needs the correct error code in the response
            Err(anyhow::anyhow!("params not an array"))
        }
        Some(params) => match params.get_mut(block_param_id) {
            None => {
                if params.len() != block_param_id - 1 {
                    // TODO: this needs the correct error code in the response
                    return Err(anyhow::anyhow!("unexpected params length"));
                }

                // add the latest block number to the end of the params
                params.push(serde_json::to_value(latest_block)?);

                Ok(latest_block)
            }
            Some(x) => {
                // convert the json value to a BlockNumber
                // TODO: this is wrong, it might be a Map like `{"blockHash": String("0xa5626dc20d3a0a209b1de85521717a3e859698de8ce98bca1b16822b7501f74b")}`
                let block_num = if let Some(obj) = x.as_object_mut() {
                    if let Some(block_hash) = obj.remove("blockHash") {
                        let block_hash: H256 = serde_json::from_value(block_hash)?;

                        todo!("look up the block_hash from our cache");
                    } else {
                        unimplemented!();
                    }
                } else {
                    serde_json::from_value::<BlockNumber>(x.take())?
                };

                let (modified, block_num) = block_num_to_u64(block_num, latest_block);

                // if we changed "latest" to a number, update the params to match
                if modified {
                    *x = serde_json::to_value(block_num)?;
                }

                Ok(block_num)
            }
        },
    }
}

// TODO: change this to also return the hash needed
pub fn block_needed(
    method: &str,
    params: Option<&mut serde_json::Value>,
    head_block_num: U64,
) -> Option<U64> {
    // if no params, no block is needed
    let params = params?;

    // get the index for the BlockNumber or return None to say no block is needed.
    // The BlockNumber is usually the last element.
    // TODO: double check these. i think some of the getBlock stuff will never need archive
    let block_param_id = match method {
        "eth_call" => 1,
        "eth_estimateGas" => 1,
        "eth_getBalance" => 1,
        "eth_getBlockByHash" => {
            // TODO: double check that any node can serve this
            return None;
        }
        "eth_getBlockByNumber" => {
            // TODO: double check that any node can serve this
            return None;
        }
        "eth_getBlockTransactionCountByHash" => {
            // TODO: double check that any node can serve this
            return None;
        }
        "eth_getBlockTransactionCountByNumber" => 0,
        "eth_getCode" => 1,
        "eth_getLogs" => {
            let obj = params[0].as_object_mut().unwrap();

            if let Some(x) = obj.get_mut("fromBlock") {
                let block_num: BlockNumber = serde_json::from_value(x.clone()).ok()?;

                let (modified, block_num) = block_num_to_u64(block_num, head_block_num);

                if modified {
                    *x = serde_json::to_value(block_num).unwrap();
                }

                return Some(block_num);
            }

            if let Some(x) = obj.get_mut("toBlock") {
                let block_num: BlockNumber = serde_json::from_value(x.clone()).ok()?;

                let (modified, block_num) = block_num_to_u64(block_num, head_block_num);

                if modified {
                    *x = serde_json::to_value(block_num).unwrap();
                }

                return Some(block_num);
            }

            if let Some(x) = obj.get("blockHash") {
                // TODO: check a Cache of recent hashes
                // TODO: error if fromBlock or toBlock were set
                todo!("handle blockHash {}", x);
            }

            return None;
        }
        "eth_getStorageAt" => 2,
        "eth_getTransactionByHash" => {
            // TODO: not sure how best to look these up
            // try full nodes first. retry will use archive
            return None;
        }
        "eth_getTransactionByBlockHashAndIndex" => {
            // TODO: check a Cache of recent hashes
            // try full nodes first. retry will use archive
            return None;
        }
        "eth_getTransactionByBlockNumberAndIndex" => 0,
        "eth_getTransactionCount" => 1,
        "eth_getTransactionReceipt" => {
            // TODO: not sure how best to look these up
            // try full nodes first. retry will use archive
            return None;
        }
        "eth_getUncleByBlockHashAndIndex" => {
            // TODO: check a Cache of recent hashes
            // try full nodes first. retry will use archive
            return None;
        }
        "eth_getUncleByBlockNumberAndIndex" => 0,
        "eth_getUncleCountByBlockHash" => {
            // TODO: check a Cache of recent hashes
            // try full nodes first. retry will use archive
            return None;
        }
        "eth_getUncleCountByBlockNumber" => 0,
        _ => {
            // some other command that doesn't take block numbers as an argument
            return None;
        }
    };

    match clean_block_number(params, block_param_id, head_block_num) {
        Ok(block) => Some(block),
        Err(err) => {
            // TODO: seems unlikely that we will get here
            warn!(?err, "could not get block from params");
            None
        }
    }
}
