//! Helper functions for turning ether's BlockNumber into numbers and updating incoming queries to match.
use crate::app::Web3ProxyApp;
use crate::jsonrpc::JsonRpcRequest;
use crate::{
    errors::{Web3ProxyError, Web3ProxyResult},
    rpcs::blockchain::Web3ProxyBlock,
};
use anyhow::Context;
use async_recursion::async_recursion;
use derive_more::From;
use ethers::{
    prelude::{BlockNumber, U64},
    types::H256,
};
use serde_json::json;
use tracing::{error, trace, warn};

#[allow(non_snake_case)]
pub fn BlockNumber_to_U64(block_num: BlockNumber, latest_block: U64) -> (U64, bool) {
    match block_num {
        BlockNumber::Earliest => (U64::zero(), false),
        BlockNumber::Finalized => {
            warn!("finalized block requested! not yet implemented!");
            (latest_block - 10, false)
        }
        BlockNumber::Latest => {
            // change "latest" to a number
            (latest_block, true)
        }
        BlockNumber::Number(x) => {
            // we already have a number
            (x, false)
        }
        BlockNumber::Pending => {
            // modified is false because we want the backend to see "pending"
            // TODO: think more about how to handle Pending
            (latest_block, false)
        }
        BlockNumber::Safe => {
            warn!("safe block requested! not yet implemented!");
            (latest_block - 3, false)
        }
    }
}

#[derive(Clone, Debug, Eq, From, Hash, PartialEq)]
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
        let n = value.number();
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
    app: &Web3ProxyApp,
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

                        let block = app
                            .balanced_rpcs
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
                        let head_block_num = latest_block.number();

                        if block_num > head_block_num {
                            return Err(Web3ProxyError::UnknownBlockNumber {
                                known: head_block_num,
                                unknown: block_num,
                            });
                        }

                        let block_hash = app
                            .balanced_rpcs
                            .block_hash(&block_num)
                            .await
                            .context("fetching block hash from number")?;

                        let block = app
                            .balanced_rpcs
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

                        if block_num == latest_block.number() {
                            (latest_block.into(), change)
                        } else {
                            let block_hash = app
                                .balanced_rpcs
                                .block_hash(&block_num)
                                .await
                                .context("fetching block hash from number")?;

                            let block = app
                                .balanced_rpcs
                                .block(&block_hash, None, None)
                                .await
                                .context("fetching block from hash")?;

                            (BlockNumAndHash::from(&block), change)
                        }
                    } else if let Ok(block_hash) = serde_json::from_value::<H256>(x.clone()) {
                        let block = app
                            .balanced_rpcs
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
/// this replaces any "latest" identifiers in the JsonRpcRequest with the current block number which feels like the data is structured wrong
#[derive(Debug, Default, Hash, Eq, PartialEq)]
pub enum CacheMode {
    CacheSuccessForever,
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
    #[default]
    CacheNever,
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
        "trace_block" => Some(0),
        "trace_call" => Some(2),
        "trace_callMany" => Some(1),
        _ => None,
    }
}

impl CacheMode {
    /// like `try_new`, but instead of erroring, it will default to caching with the head block
    /// returns None if this request should not be cached
    #[async_recursion]
    pub async fn new<'a>(
        request: &'a mut JsonRpcRequest,
        head_block: Option<&'a Web3ProxyBlock>,
        app: Option<&'a Web3ProxyApp>,
    ) -> Self {
        match Self::try_new(request, head_block, app).await {
            Ok(x) => x,
            Err(Web3ProxyError::NoBlocksKnown) => {
                warn!(
                    method = %request.method,
                    params = ?request.params,
                    "no servers available to get block from params. caching with head block"
                );
                if let Some(head_block) = head_block {
                    // TODO: strange to get NoBlocksKnown **and** have a head block. think about this more
                    CacheMode::Cache {
                        block: head_block.into(),
                        cache_errors: true,
                    }
                } else {
                    CacheMode::CacheNever
                }
            }
            Err(err) => {
                error!(
                    method = %request.method,
                    params = ?request.params,
                    ?err,
                    "could not get block from params. caching with head block"
                );
                if let Some(head_block) = head_block {
                    CacheMode::Cache {
                        block: head_block.into(),
                        cache_errors: true,
                    }
                } else {
                    CacheMode::CacheNever
                }
            }
        }
    }

    pub async fn try_new(
        request: &mut JsonRpcRequest,
        head_block: Option<&Web3ProxyBlock>,
        app: Option<&Web3ProxyApp>,
    ) -> Web3ProxyResult<Self> {
        let params = &mut request.params;

        if matches!(params, serde_json::Value::Null) {
            // no params given. cache with the head block
            if let Some(head_block) = head_block {
                return Ok(Self::Cache {
                    block: head_block.into(),
                    cache_errors: true,
                });
            } else {
                return Ok(Self::CacheNever);
            }
        }

        if head_block.is_none() {
            // since we don't have a head block, i don't trust our anything enough to cache
            return Ok(Self::CacheNever);
        }

        let head_block = head_block.expect("head_block was just checked above");

        if let Some(params) = params.as_array() {
            if params.is_empty() {
                // no params given. cache with the head block
                return Ok(Self::Cache {
                    block: head_block.into(),
                    cache_errors: true,
                });
            }
        }

        match request.method.as_str() {
            "debug_traceTransaction" => {
                // TODO: make sure re-orgs work properly!
                Ok(CacheMode::CacheSuccessForever)
            }
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
                /*
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

                        let block_hash = rpcs.block_hash(&block_num).await?;

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

                        let block_hash = rpcs.block_hash(&block_num).await?;

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
                */
                Ok(CacheMode::Cache {
                    block: head_block.into(),
                    cache_errors: true,
                })
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
            "net_listening" => Ok(CacheMode::CacheSuccessForever),
            "net_version" => Ok(CacheMode::CacheSuccessForever),
            method => match get_block_param_id(method) {
                Some(block_param_id) => {
                    if let Some(app) = app {
                        let block =
                            clean_block_number(params, block_param_id, head_block, app).await?;

                        Ok(CacheMode::Cache {
                            block,
                            cache_errors: true,
                        })
                    } else {
                        Ok(CacheMode::CacheNever)
                    }
                }
                None => Err(Web3ProxyError::UnhandledMethod(method.to_string().into())),
            },
        }
    }

    pub fn cache_jsonrpc_errors(&self) -> bool {
        match self {
            Self::CacheNever => false,
            Self::CacheSuccessForever => true,
            Self::Cache { cache_errors, .. } => *cache_errors,
            Self::CacheRange { cache_errors, .. } => *cache_errors,
        }
    }

    pub fn from_block(&self) -> Option<&BlockNumAndHash> {
        match self {
            Self::CacheSuccessForever => None,
            Self::CacheNever => None,
            Self::Cache { block, .. } => Some(block),
            Self::CacheRange { from_block, .. } => Some(from_block),
        }
    }

    #[inline]
    pub fn is_some(&self) -> bool {
        !matches!(self, Self::CacheNever)
    }

    pub fn to_block(&self) -> Option<&BlockNumAndHash> {
        match self {
            Self::CacheSuccessForever => None,
            Self::CacheNever => None,
            Self::Cache { block, .. } => Some(block),
            Self::CacheRange { to_block, .. } => Some(to_block),
        }
    }
}

#[cfg(test)]
mod test {
    use super::CacheMode;
    use crate::{
        jsonrpc::{JsonRpcId, JsonRpcRequest},
        rpcs::{blockchain::Web3ProxyBlock, many::Web3Rpcs},
    };
    use ethers::types::{Block, H256};
    use serde_json::json;
    use std::sync::Arc;

    #[test_log::test(tokio::test)]
    async fn test_fee_history() {
        let method = "eth_feeHistory";
        let params = json!([4, "latest", [25, 75]]);

        let head_block = Block {
            number: Some(1.into()),
            hash: Some(H256::random()),
            ..Default::default()
        };

        let head_block = Web3ProxyBlock::try_new(Arc::new(head_block)).unwrap();

        let (empty, _handle, _ranked_rpc_reciver) =
            Web3Rpcs::spawn(1, None, 1, 1, "test".into(), None, None)
                .await
                .unwrap();

        let id = JsonRpcId::Number(9);

        let mut request = JsonRpcRequest::new(id, method.to_string(), params).unwrap();

        // TODO: instead of empty, check None?
        let x = CacheMode::try_new(&mut request, Some(&head_block), None)
            .await
            .unwrap();

        assert_eq!(
            x,
            CacheMode::Cache {
                block: (&head_block).into(),
                cache_errors: true
            }
        );

        // "latest" should have been changed to the block number
        assert_eq!(request.params.get(1), Some(&json!(head_block.number())));
    }

    #[test_log::test(tokio::test)]
    async fn test_eth_call_latest() {
        let method = "eth_call";

        let params = json!([{"data": "0xdeadbeef", "to": "0x0000000000000000000000000000000000000000"}, "latest"]);

        let head_block = Block {
            number: Some(18173997.into()),
            hash: Some(H256::random()),
            ..Default::default()
        };

        let head_block = Web3ProxyBlock::try_new(Arc::new(head_block)).unwrap();

        let id = JsonRpcId::Number(99);

        let mut request = JsonRpcRequest::new(id, method.to_string(), params).unwrap();

        let (empty, _handle, _ranked_rpc_reciver) =
            Web3Rpcs::spawn(1, None, 1, 1, "test".into(), None, None)
                .await
                .unwrap();

        let x = CacheMode::try_new(&mut request, Some(&head_block), None)
            .await
            .unwrap();

        // "latest" should have been changed to the block number
        assert_eq!(request.params.get(1), Some(&json!(head_block.number())));

        assert_eq!(
            x,
            CacheMode::Cache {
                block: (&head_block).into(),
                cache_errors: true
            }
        );
    }
}
