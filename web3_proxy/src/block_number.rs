//! Helper functions for turning ether's BlockNumber into numbers and updating incoming queries to match.
use anyhow::Context;
use ethers::{
    prelude::{BlockNumber, U64},
    types::H256,
};
use log::{trace, warn};
use serde_json::json;
use std::sync::Arc;

use crate::{frontend::authorization::Authorization, rpcs::connections::Web3Connections};

pub fn block_num_to_U64(block_num: BlockNumber, latest_block: U64) -> U64 {
    match block_num {
        BlockNumber::Earliest => {
            // modified is false because we want the backend to see "pending"
            U64::zero()
        }
        BlockNumber::Finalized => {
            warn!("finalized block requested! not yet implemented!");
            latest_block - 10
        }
        BlockNumber::Latest => {
            // change "latest" to a number
            latest_block
        }
        BlockNumber::Number(x) => {
            // we already have a number
            x
        }
        BlockNumber::Pending => {
            // TODO: think more about how to handle Pending
            latest_block
        }
        BlockNumber::Safe => {
            warn!("finalized block requested! not yet implemented!");
            latest_block - 3
        }
    }
}

/// modify params to always have a block number and not "latest"

pub async fn clean_block_number(
    authorization: &Arc<Authorization>,
    params: &mut serde_json::Value,
    block_param_id: usize,
    latest_block: U64,
    rpcs: &Web3Connections,
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
                let start = x.clone();

                // convert the json value to a BlockNumber
                let block_num = if let Some(obj) = x.as_object_mut() {
                    // it might be a Map like `{"blockHash": String("0xa5626dc20d3a0a209b1de85521717a3e859698de8ce98bca1b16822b7501f74b")}`
                    if let Some(block_hash) = obj.remove("blockHash") {
                        let block_hash: H256 =
                            serde_json::from_value(block_hash).context("decoding blockHash")?;

                        let block = rpcs.block(authorization, &block_hash, None).await?;

                        block
                            .number
                            .expect("blocks here should always have numbers")
                    } else {
                        return Err(anyhow::anyhow!("blockHash missing"));
                    }
                } else {
                    // it might be a string like "latest" or a block number
                    // TODO: "BlockNumber" needs a better name
                    let block_number = serde_json::from_value::<BlockNumber>(x.take())?;

                    block_num_to_U64(block_number, latest_block)
                };

                // if we changed "latest" to a number, update the params to match
                *x = serde_json::to_value(block_num)?;

                // TODO: only do this if trace logging is enabled
                if x.as_u64() != start.as_u64() {
                    trace!("changed {} to {}", start, x);
                }

                Ok(block_num)
            }
        },
    }
}

/// TODO: change this to also return the hash needed?
pub enum BlockNeeded {
    CacheSuccessForever,
    CacheNever,
    Cache { block_num: U64, cache_errors: bool },
}

pub async fn block_needed(
    authorization: &Arc<Authorization>,
    method: &str,
    params: Option<&mut serde_json::Value>,
    head_block_num: U64,
    rpcs: &Web3Connections,
) -> anyhow::Result<BlockNeeded> {
    // if no params, no block is needed
    let params = if let Some(params) = params {
        params
    } else {
        // TODO: check all the methods with no params, some might not be cacheable
        // caching for one block should always be okay
        return Ok(BlockNeeded::Cache {
            block_num: head_block_num,
            cache_errors: true,
        });
    };

    // get the index for the BlockNumber or return None to say no block is needed.
    // The BlockNumber is usually the last element.
    // TODO: double check these. i think some of the getBlock stuff will never need archive
    let block_param_id = match method {
        "eth_call" => 1,
        "eth_estimateGas" => 1,
        "eth_getBalance" => 1,
        "eth_getBlockByHash" => {
            // TODO: double check that any node can serve this
            // TODO: can a block change? like what if it gets orphaned?
            return Ok(BlockNeeded::CacheSuccessForever);
        }
        "eth_getBlockByNumber" => {
            // TODO: double check that any node can serve this
            // TODO: CacheSuccessForever if the block is old enough
            return Ok(BlockNeeded::Cache {
                block_num: head_block_num,
                cache_errors: true,
            });
        }
        "eth_getBlockReceipts" => 0,
        "eth_getBlockTransactionCountByHash" => {
            // TODO: double check that any node can serve this
            return Ok(BlockNeeded::CacheSuccessForever);
        }
        "eth_getBlockTransactionCountByNumber" => 0,
        "eth_getCode" => 1,
        "eth_getLogs" => {
            // TODO: think about this more
            // TODO: jsonrpc has a specific code for this
            let obj = params[0]
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("invalid format"))?;

            if let Some(x) = obj.get_mut("fromBlock") {
                let block_num: BlockNumber = serde_json::from_value(x.take())?;

                let block_num = block_num_to_U64(block_num, head_block_num);

                *x = json!(block_num);

                // TODO: maybe don't return. instead check toBlock too?
                // TODO: if there is a very wide fromBlock and toBlock, we need to check that our rpcs have both!
                return Ok(BlockNeeded::Cache {
                    block_num,
                    cache_errors: false,
                });
            }

            if let Some(x) = obj.get_mut("toBlock") {
                let block_num: BlockNumber = serde_json::from_value(x.take())?;

                let block_num = block_num_to_U64(block_num, head_block_num);

                *x = json!(block_num);

                return Ok(BlockNeeded::Cache {
                    block_num,
                    cache_errors: false,
                });
            }

            if obj.contains_key("blockHash") {
                1
            } else {
                return Ok(BlockNeeded::Cache {
                    block_num: head_block_num,
                    cache_errors: true,
                });
            }
        }
        "eth_getStorageAt" => 2,
        "eth_getTransactionByHash" => {
            // TODO: not sure how best to look these up
            // try full nodes first. retry will use archive
            return Ok(BlockNeeded::Cache {
                block_num: head_block_num,
                cache_errors: true,
            });
        }
        "eth_getTransactionByBlockHashAndIndex" => {
            // TODO: check a Cache of recent hashes
            // try full nodes first. retry will use archive
            return Ok(BlockNeeded::CacheSuccessForever);
        }
        "eth_getTransactionByBlockNumberAndIndex" => 0,
        "eth_getTransactionCount" => 1,
        "eth_getTransactionReceipt" => {
            // TODO: not sure how best to look these up
            // try full nodes first. retry will use archive
            return Ok(BlockNeeded::Cache {
                block_num: head_block_num,
                cache_errors: true,
            });
        }
        "eth_getUncleByBlockHashAndIndex" => {
            // TODO: check a Cache of recent hashes
            // try full nodes first. retry will use archive
            return Ok(BlockNeeded::CacheSuccessForever);
        }
        "eth_getUncleByBlockNumberAndIndex" => 0,
        "eth_getUncleCountByBlockHash" => {
            // TODO: check a Cache of recent hashes
            // try full nodes first. retry will use archive
            return Ok(BlockNeeded::CacheSuccessForever);
        }
        "eth_getUncleCountByBlockNumber" => 0,
        _ => {
            // some other command that doesn't take block numbers as an argument
            // since we are caching with the head block, it should be safe to cache_errors
            return Ok(BlockNeeded::Cache {
                block_num: head_block_num,
                cache_errors: true,
            });
        }
    };

    match clean_block_number(authorization, params, block_param_id, head_block_num, rpcs).await {
        Ok(block_num) => Ok(BlockNeeded::Cache {
            block_num,
            cache_errors: true,
        }),
        Err(err) => {
            warn!("could not get block from params. err={:?}", err);
            Ok(BlockNeeded::Cache {
                block_num: head_block_num,
                cache_errors: true,
            })
        }
    }
}
