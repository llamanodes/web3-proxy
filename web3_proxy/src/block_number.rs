//! Helper functions for turning ether's BlockNumber into numbers and updating incoming queries to match.
use crate::{
    errors::{Web3ProxyError, Web3ProxyResult},
    rpcs::blockchain::Web3ProxyBlock,
};
use anyhow::Context;
use derive_more::From;
use ethers::{
    prelude::{BlockNumber, U64},
    types::H256,
};
use serde_json::json;
use std::sync::Arc;
use tracing::{error, trace, warn};

use crate::{frontend::authorization::Authorization, rpcs::many::Web3Rpcs};

#[allow(non_snake_case)]
pub fn BlockNumber_to_U64(block_num: BlockNumber, latest_block: &U64) -> (U64, bool) {
    match block_num {
        BlockNumber::Earliest => (U64::zero(), false),
        BlockNumber::Finalized => {
            warn!("finalized block requested! not yet implemented!");
            (*latest_block - 10, false)
        }
        BlockNumber::Latest => {
            // change "latest" to a number
            (*latest_block, true)
        }
        BlockNumber::Number(x) => {
            // we already have a number
            (x, false)
        }
        BlockNumber::Pending => {
            // modified is false because we want the backend to see "pending"
            // TODO: think more about how to handle Pending
            (*latest_block, false)
        }
        BlockNumber::Safe => {
            warn!("safe block requested! not yet implemented!");
            (*latest_block - 3, false)
        }
    }
}

#[derive(Clone, Debug, Eq, From, PartialEq)]
pub struct BlockNumAndHash(U64, H256);

impl BlockNumAndHash {
    pub fn num(&self) -> &U64 {
        &self.0
    }
    pub fn hash(&self) -> &H256 {
        &self.1
    }
}

impl From<&Web3ProxyBlock> for BlockNumAndHash {
    fn from(value: &Web3ProxyBlock) -> Self {
        let n = *value.number();
        let h = *value.hash();

        Self(n, h)
    }
}

/// modify params to always have a block hash and not "latest"
/// TODO: this should replace all block numbers with hashes, not just "latest"
pub async fn clean_block_number(
    authorization: &Arc<Authorization>,
    params: &mut serde_json::Value,
    block_param_id: usize,
    latest_block: &Web3ProxyBlock,
    rpcs: &Web3Rpcs,
) -> Web3ProxyResult<BlockNumAndHash> {
    match params.as_array_mut() {
        None => {
            // TODO: this needs the correct error code in the response
            Err(anyhow::anyhow!("params not an array").into())
        }
        Some(params) => match params.get_mut(block_param_id) {
            None => {
                if params.len() == block_param_id {
                    // add the latest block number to the end of the params
                    params.push(json!(latest_block));
                } else {
                    // don't modify the request. only cache with current block
                    // TODO: more useful log that include the
                    warn!("unexpected params length");
                }

                // don't modify params, just cache with the current block
                Ok(latest_block.into())
            }
            Some(x) => {
                // convert the json value to a BlockNumber
                let (block, change) = if let Some(obj) = x.as_object_mut() {
                    // it might be a Map like `{"blockHash": String("0xa5626dc20d3a0a209b1de85521717a3e859698de8ce98bca1b16822b7501f74b")}`
                    if let Some(block_hash) = obj.get("blockHash").cloned() {
                        let block_hash: H256 =
                            serde_json::from_value(block_hash).context("decoding blockHash")?;

                        let block = rpcs
                            .block(authorization, &block_hash, None, Some(3), None)
                            .await
                            .context("fetching block number from hash")?;

                        (BlockNumAndHash::from(&block), false)
                    } else {
                        return Err(anyhow::anyhow!("blockHash missing").into());
                    }
                } else {
                    // it might be a string like "latest" or a block number or a block hash
                    // TODO: "BlockNumber" needs a better name
                    // TODO: move this to a helper function?
                    if let Ok(block_num) = serde_json::from_value::<U64>(x.clone()) {
                        let (block_hash, _) = rpcs
                            .block_hash(authorization, &block_num)
                            .await
                            .context("fetching block hash from number")?;

                        let block = rpcs
                            .block(authorization, &block_hash, None, Some(3), None)
                            .await
                            .context("fetching block from hash")?;

                        // TODO: do true here? will that work for **all** methods on **all** chains? if not we need something smarter
                        (BlockNumAndHash::from(&block), false)
                    } else if let Ok(block_number) =
                        serde_json::from_value::<BlockNumber>(x.clone())
                    {
                        let (block_num, change) =
                            BlockNumber_to_U64(block_number, latest_block.number());

                        if block_num == *latest_block.number() {
                            (latest_block.into(), change)
                        } else {
                            let (block_hash, _) = rpcs
                                .block_hash(authorization, &block_num)
                                .await
                                .context("fetching block hash from number")?;

                            let block = rpcs
                                .block(authorization, &block_hash, None, Some(3), None)
                                .await
                                .context("fetching block from hash")?;

                            (BlockNumAndHash::from(&block), change)
                        }
                    } else if let Ok(block_hash) = serde_json::from_value::<H256>(x.clone()) {
                        let block = rpcs
                            .block(authorization, &block_hash, None, Some(3), None)
                            .await
                            .context("fetching block number from hash")?;

                        (BlockNumAndHash::from(&block), false)
                    } else {
                        return Err(anyhow::anyhow!(
                            "param not a block identifier, block number, or block hash"
                        )
                        .into());
                    }
                };

                // if we changed "latest" to a hash, update the params to match
                if change {
                    trace!(old=%x, new=%block.hash(), "changing block number");
                    *x = json!(block.hash());
                }

                Ok(block)
            }
        },
    }
}

/// TODO: change this to also return the hash needed?
pub enum CacheMode {
    CacheSuccessForever,
    CacheNever,
    Cache {
        block: BlockNumAndHash,
        /// cache jsonrpc errors (server errors are never cached)
        cache_errors: bool,
    },
    CacheRange {
        from_block: BlockNumAndHash,
        to_block: BlockNumAndHash,
        /// cache jsonrpc errors (server errors are never cached)
        cache_errors: bool,
    },
}

impl CacheMode {
    pub async fn new(
        authorization: &Arc<Authorization>,
        method: &str,
        params: &mut serde_json::Value,
        head_block: &Web3ProxyBlock,
        rpcs: &Web3Rpcs,
    ) -> Self {
        match Self::try_new(authorization, method, params, head_block, rpcs).await {
            Ok(x) => x,
            Err(err) => {
                warn!(?err, "unable to determine cache mode from params");
                Self::CacheNever
            }
        }
    }

    pub async fn try_new(
        authorization: &Arc<Authorization>,
        method: &str,
        params: &mut serde_json::Value,
        head_block: &Web3ProxyBlock,
        rpcs: &Web3Rpcs,
    ) -> Web3ProxyResult<Self> {
        // some requests have potentially very large responses
        // TODO: only skip caching if the response actually is large
        if method.starts_with("trace_") || method == "debug_traceTransaction" {
            return Ok(Self::CacheNever);
        }

        if matches!(params, serde_json::Value::Null) {
            // no params given
            return Ok(Self::Cache {
                block: head_block.into(),
                cache_errors: true,
            });
        }

        // get the index for the BlockNumber
        // The BlockNumber is usually the last element.
        // TODO: double check these. i think some of the getBlock stuff will never need archive
        let block_param_id = match method {
            "eth_call" => 1,
            "eth_estimateGas" => 1,
            "eth_getBalance" => 1,
            "eth_getBlockByHash" => {
                // TODO: double check that any node can serve this
                // TODO: can a block change? like what if it gets orphaned?
                return Ok(CacheMode::CacheSuccessForever);
            }
            "eth_getBlockByNumber" => {
                // TODO: double check that any node can serve this
                // TODO: CacheSuccessForever if the block is old enough
                return Ok(CacheMode::Cache {
                    block: head_block.into(),
                    cache_errors: true,
                });
            }
            "eth_getBlockReceipts" => 0,
            "eth_getBlockTransactionCountByHash" => {
                // TODO: double check that any node can serve this
                return Ok(CacheMode::CacheSuccessForever);
            }
            "eth_getBlockTransactionCountByNumber" => 0,
            "eth_getCode" => 1,
            "eth_getLogs" => {
                // TODO: think about this more
                // TODO: jsonrpc has a specific code for this
                let obj = params
                    .get_mut(0)
                    .ok_or_else(|| Web3ProxyError::BadRequest("invalid format. no params".into()))?
                    .as_object_mut()
                    .ok_or_else(|| {
                        Web3ProxyError::BadRequest("invalid format. params not object".into())
                    })?;

                if obj.contains_key("blockHash") {
                    return Ok(CacheMode::CacheSuccessForever);
                } else {
                    let from_block = if let Some(x) = obj.get_mut("fromBlock") {
                        // TODO: use .take instead of clone
                        // what if its a hash?
                        let block_num: BlockNumber = serde_json::from_value(x.clone())?;

                        let (block_num, change) =
                            BlockNumber_to_U64(block_num, head_block.number());

                        if change {
                            // TODO: include the hash instead of the number?
                            trace!("changing fromBlock in eth_getLogs. {} -> {}", x, block_num);
                            *x = json!(block_num);
                        }

                        let (block_hash, _) = rpcs.block_hash(authorization, &block_num).await?;

                        BlockNumAndHash(block_num, block_hash)
                    } else {
                        BlockNumAndHash(U64::zero(), H256::zero())
                    };

                    let to_block = if let Some(x) = obj.get_mut("toBlock") {
                        // TODO: use .take instead of clone
                        // what if its a hash?
                        let block_num: BlockNumber = serde_json::from_value(x.clone())?;

                        let (block_num, change) =
                            BlockNumber_to_U64(block_num, head_block.number());

                        if change {
                            trace!("changing toBlock in eth_getLogs. {} -> {}", x, block_num);
                            *x = json!(block_num);
                        }

                        let (block_hash, _) = rpcs.block_hash(authorization, &block_num).await?;

                        BlockNumAndHash(block_num, block_hash)
                    } else {
                        head_block.into()
                    };

                    return Ok(CacheMode::CacheRange {
                        from_block,
                        to_block,
                        cache_errors: true,
                    });
                }
            }
            "eth_getStorageAt" => 2,
            "eth_getTransactionByHash" => {
                // TODO: not sure how best to look these up
                // try full nodes first. retry will use archive
                return Ok(CacheMode::Cache {
                    block: head_block.into(),
                    cache_errors: true,
                });
            }
            "eth_getTransactionByBlockHashAndIndex" => {
                // TODO: check a Cache of recent hashes
                // try full nodes first. retry will use archive
                return Ok(CacheMode::CacheSuccessForever);
            }
            "eth_getTransactionByBlockNumberAndIndex" => 0,
            "eth_getTransactionCount" => 1,
            "eth_getTransactionReceipt" => {
                // TODO: not sure how best to look these up
                // try full nodes first. retry will use archive
                return Ok(CacheMode::Cache {
                    block: head_block.into(),
                    cache_errors: true,
                });
            }
            "eth_getUncleByBlockHashAndIndex" => {
                // TODO: check a Cache of recent hashes
                // try full nodes first. retry will use archive
                // TODO: what happens if this block is uncled later?
                return Ok(CacheMode::CacheSuccessForever);
            }
            "eth_getUncleByBlockNumberAndIndex" => 0,
            "eth_getUncleCountByBlockHash" => {
                // TODO: check a Cache of recent hashes
                // try full nodes first. retry will use archive
                // TODO: what happens if this block is uncled later?
                return Ok(CacheMode::CacheSuccessForever);
            }
            "eth_getUncleCountByBlockNumber" => 0,
            _ => {
                // some other command that doesn't take block numbers as an argument
                // since we are caching with the head block, it should be safe to cache_errors
                return Ok(CacheMode::Cache {
                    block: head_block.into(),
                    cache_errors: true,
                });
            }
        };

        match clean_block_number(authorization, params, block_param_id, head_block, rpcs).await {
            Ok(block) => Ok(CacheMode::Cache {
                block,
                cache_errors: true,
            }),
            Err(Web3ProxyError::NoBlocksKnown) => {
                warn!(%method, ?params, "no servers available to get block from params");
                Ok(CacheMode::Cache {
                    block: head_block.into(),
                    cache_errors: true,
                })
            }
            Err(err) => {
                error!(%method, ?params, ?err, "could not get block from params");
                Ok(CacheMode::Cache {
                    block: head_block.into(),
                    cache_errors: true,
                })
            }
        }
    }
}
