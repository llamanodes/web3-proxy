//! Helper functions for turning ether's BlockNumber into numbers and updating incoming queries to match.
use crate::rpcs::many::Web3Rpcs;
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
use tracing::{error, trace, warn};

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
                    params.push(json!(latest_block.number()));
                } else {
                    // don't modify the request. only cache with current block
                    // TODO: more useful log that include the
                    warn!("unexpected params length");
                }

                // don't modify params, just cache with the current block
                Ok(latest_block.into())
            }
            Some(x) => {
                // dig into the json value to find a BlockNumber or similar block identifier
                trace!(?x, "inspecting");

                let (block, change) = if let Some(obj) = x.as_object_mut() {
                    // it might be a Map like `{"blockHash": String("0xa5626dc20d3a0a209b1de85521717a3e859698de8ce98bca1b16822b7501f74b")}`
                    if let Some(block_hash) = obj.get("blockHash").cloned() {
                        let block_hash: H256 =
                            serde_json::from_value(block_hash).context("decoding blockHash")?;

                        let block = rpcs
                            .block(&block_hash, None, None)
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
                            .block_hash(&block_num)
                            .await
                            .context("fetching block hash from number")?;

                        let block = rpcs
                            .block(&block_hash, None, None)
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
                                .block_hash(&block_num)
                                .await
                                .context("fetching block hash from number")?;

                            let block = rpcs
                                .block(&block_hash, None, None)
                                .await
                                .context("fetching block from hash")?;

                            (BlockNumAndHash::from(&block), change)
                        }
                    } else if let Ok(block_hash) = serde_json::from_value::<H256>(x.clone()) {
                        let block = rpcs
                            .block(&block_hash, None, None)
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

                // if we changed "latest" to an actual block, update the params to match
                // TODO: should we do hash or number? some functions work with either, but others need a number :cry:
                if change {
                    trace!(old=%x, new=%block.num(), "changing block number");
                    *x = json!(block.num());
                }

                Ok(block)
            }
        },
    }
}

/// TODO: change this to also return the hash needed?
#[derive(Debug, Eq, PartialEq)]
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

fn get_block_param_id(method: &str) -> Option<usize> {
    match method {
        "debug_traceBlockByHash" => Some(0),
        "debug_traceBlockByNumber" => Some(0),
        "debug_traceCall" => Some(1),
        "debug_traceTransaction" => None,
        "eth_call" => Some(1),
        "eth_estimateGas" => Some(1),
        "eth_feeHistory" => Some(1),
        "eth_getBalance" => Some(1),
        "eth_getBlockReceipts" => Some(0),
        "eth_getBlockTransactionCountByNumber" => Some(0),
        "eth_getCode" => Some(1),
        "eth_getStorageAt" => Some(2),
        "eth_getTransactionByBlockNumberAndIndex" => Some(0),
        "eth_getTransactionCount" => Some(1),
        "eth_getUncleByBlockNumberAndIndex" => Some(0),
        "eth_getUncleCountByBlockNumber" => Some(0),
        "trace_call" => Some(2),
        _ => None,
    }
}

impl CacheMode {
    pub async fn new(
        method: &str,
        params: &mut serde_json::Value,
        head_block: &Web3ProxyBlock,
        rpcs: &Web3Rpcs,
    ) -> Self {
        match Self::try_new(method, params, head_block, rpcs).await {
            Ok(x) => x,
            Err(Web3ProxyError::NoBlocksKnown) => {
                warn!(%method, ?params, "no servers available to get block from params. caching with head block");
                CacheMode::Cache {
                    block: head_block.into(),
                    cache_errors: true,
                }
            }
            Err(err) => {
                error!(%method, ?params, ?err, "could not get block from params. caching with head block");
                CacheMode::Cache {
                    block: head_block.into(),
                    cache_errors: true,
                }
            }
        }
    }

    pub async fn try_new(
        method: &str,
        params: &mut serde_json::Value,
        head_block: &Web3ProxyBlock,
        rpcs: &Web3Rpcs,
    ) -> Web3ProxyResult<Self> {
        if matches!(params, serde_json::Value::Null) {
            // no params given. cache with the head block
            return Ok(Self::Cache {
                block: head_block.into(),
                cache_errors: true,
            });
        }

        match method {
            "eth_gasPrice" => Ok(CacheMode::Cache {
                block: head_block.into(),
                cache_errors: false,
            }),
            "eth_getBlockByHash" => {
                // TODO: double check that any node can serve this
                // TODO: can a block change? like what if it gets orphaned?
                // TODO: make sure re-orgs work properly!
                Ok(CacheMode::CacheSuccessForever)
            }
            "eth_getBlockByNumber" => {
                // TODO: double check that any node can serve this
                // TODO: CacheSuccessForever if the block is old enough
                // TODO: make sure re-orgs work properly!
                Ok(CacheMode::Cache {
                    block: head_block.into(),
                    cache_errors: true,
                })
            }
            "eth_getBlockTransactionCountByHash" => {
                // TODO: double check that any node can serve this
                Ok(CacheMode::CacheSuccessForever)
            }
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
                    Ok(CacheMode::CacheSuccessForever)
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

                        let (block_hash, _) = rpcs.block_hash(&block_num).await?;

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

                        let (block_hash, _) = rpcs.block_hash(&block_num).await?;

                        BlockNumAndHash(block_num, block_hash)
                    } else {
                        head_block.into()
                    };

                    Ok(CacheMode::CacheRange {
                        from_block,
                        to_block,
                        cache_errors: true,
                    })
                }
            }
            "eth_getTransactionByHash" => {
                // TODO: not sure how best to look these up
                // try full nodes first. retry will use archive
                Ok(CacheMode::Cache {
                    block: head_block.into(),
                    cache_errors: true,
                })
            }
            "eth_getTransactionByBlockHashAndIndex" => {
                // TODO: check a Cache of recent hashes
                // try full nodes first. retry will use archive
                Ok(CacheMode::CacheSuccessForever)
            }
            "eth_getTransactionReceipt" => {
                // TODO: not sure how best to look these up
                // try full nodes first. retry will use archive
                Ok(CacheMode::Cache {
                    block: head_block.into(),
                    cache_errors: true,
                })
            }
            "eth_getUncleByBlockHashAndIndex" => {
                // TODO: check a Cache of recent hashes
                // try full nodes first. retry will use archive
                // TODO: what happens if this block is uncled later?
                Ok(CacheMode::CacheSuccessForever)
            }
            "eth_getUncleCountByBlockHash" => {
                // TODO: check a Cache of recent hashes
                // try full nodes first. retry will use archive
                // TODO: what happens if this block is uncled later?
                Ok(CacheMode::CacheSuccessForever)
            }
            "eth_maxPriorityFeePerGas" => {
                // TODO: this might be too aggressive. i think it can change before a block is mined
                Ok(CacheMode::Cache {
                    block: head_block.into(),
                    cache_errors: false,
                })
            }
            method => match get_block_param_id(method) {
                Some(block_param_id) => {
                    let block =
                        clean_block_number(params, block_param_id, head_block, rpcs).await?;

                    Ok(CacheMode::Cache {
                        block,
                        cache_errors: true,
                    })
                }
                None => Err(Web3ProxyError::UnhandledMethod(method.to_string().into())),
            },
        }
    }
}

#[cfg(test)]
mod test {
    use super::CacheMode;
    use crate::rpcs::{blockchain::Web3ProxyBlock, many::Web3Rpcs};
    use ethers::types::{Block, H256};
    use serde_json::json;
    use std::sync::Arc;

    #[test_log::test(tokio::test)]
    async fn test_fee_history() {
        let method = "eth_feeHistory";
        let mut params = json!([4, "latest", [25, 75]]);

        let head_block = Block {
            number: Some(1.into()),
            hash: Some(H256::random()),
            ..Default::default()
        };

        let head_block = Web3ProxyBlock::try_new(Arc::new(head_block)).unwrap();

        let (empty, _handle, _ranked_rpc_reciver) =
            Web3Rpcs::spawn(1, None, 1, 1, "test".into(), None)
                .await
                .unwrap();

        let x = CacheMode::try_new(method, &mut params, &head_block, &empty)
            .await
            .unwrap();

        assert_eq!(
            x,
            CacheMode::Cache {
                block: (&head_block).into(),
                cache_errors: true
            }
        );

        assert_eq!(params.get(1), Some(&json!(head_block.number())));
    }
}
