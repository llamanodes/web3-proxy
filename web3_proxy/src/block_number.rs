//! Helper functions for turning ether's BlockNumber into numbers and updating incoming queries to match.
use crate::app::App;
use crate::jsonrpc::SingleRequest;
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
    #[inline]
    pub fn num(&self) -> U64 {
        self.0
    }
    #[inline]
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
/// TODO: it would be nice to replace "latest" with the hash, but not all methods support that
pub async fn clean_block_number<'a>(
    params: &'a mut serde_json::Value,
    block_param_id: usize,
    head_block: &'a Web3ProxyBlock,
    app: Option<&'a App>,
) -> Web3ProxyResult<BlockNumOrHash> {
    match params.as_array_mut() {
        None => {
            // TODO: this needs the correct error code in the response
            Err(anyhow::anyhow!("params not an array").into())
        }
        Some(params) => match params.get_mut(block_param_id) {
            None => {
                if params.len() == block_param_id {
                    // add the latest block number to the end of the params
                    params.push(json!(head_block.number()));

                    Ok(head_block.into())
                } else {
                    // don't modify the request
                    Err(Web3ProxyError::BadRequest(
                        "unexpected params length".into(),
                    ))
                }
            }
            Some(x) => {
                // dig into the json value to find a BlockNumber or similar block identifier
                trace!(?x, "inspecting");

                let (block, change) = if let Some(obj) = x.as_object_mut() {
                    // it might be a Map like `{"blockHash": String("0xa5626dc20d3a0a209b1de85521717a3e859698de8ce98bca1b16822b7501f74b")}`
                    if let Some(block_hash) = obj.get("blockHash").cloned() {
                        let block_hash: H256 =
                            serde_json::from_value(block_hash).context("decoding blockHash")?;

                        if block_hash == *head_block.hash() {
                            (head_block.into(), false)
                        } else if let Some(app) = app {
                            // TODO: make a jsonrpc query here? cache rates will be better but it adds a network request
                            let block = app
                                .balanced_rpcs
                                .blocks_by_hash
                                .get(&block_hash)
                                .await
                                .context("fetching block number from hash")?;

                            (BlockNumOrHash::from(&block), false)
                        } else {
                            return Err(anyhow::anyhow!(
                                "app missing. cannot find block number from hash"
                            )
                            .into());
                        }
                    } else {
                        return Err(anyhow::anyhow!("blockHash missing").into());
                    }
                } else {
                    // it might be a string like "latest" or a block number or a block hash
                    // TODO: "BlockNumber" needs a better name
                    // TODO: move this to a helper function?
                    let (block_num, changed) = if let Some(block_num) = x.as_u64() {
                        (U64::from(block_num), false)
                    } else if let Ok(block_num) = serde_json::from_value::<U64>(x.to_owned()) {
                        (block_num, false)
                    } else if let Ok(block_number) =
                        serde_json::from_value::<BlockNumber>(x.to_owned())
                    {
                        BlockNumber_to_U64(block_number, head_block.number())
                    } else if let Ok(block_hash) = serde_json::from_value::<H256>(x.clone()) {
                        if block_hash == *head_block.hash() {
                            (head_block.number(), false)
                        } else if let Some(app) = app {
                            // TODO: make a jsonrpc query here? cache rates will be better but it adds a network request
                            let block = app
                                .balanced_rpcs
                                .blocks_by_hash
                                .get(&block_hash)
                                .await
                                .context("fetching block number from hash")?;

                            (block.number(), false)
                        } else {
                            return Err(anyhow::anyhow!(
                                "app missing. cannot find block number from hash"
                            )
                            .into());
                        }
                    } else {
                        return Err(anyhow::anyhow!(
                            "param not a block identifier, block number, or block hash"
                        )
                        .into());
                    };

                    let head_block_num = head_block.number();

                    if block_num > head_block_num {
                        // todo!(option to wait for the block here)
                        return Err(Web3ProxyError::UnknownBlockNumber {
                            known: head_block_num,
                            unknown: block_num,
                        });
                    }

                    if block_num == head_block_num {
                        (head_block.into(), changed)
                    } else if let Some(app) = app {
                        // TODO: make a jsonrpc query here? cache rates will be better but it adds a network request
                        if let Some(block_hash) =
                            app.balanced_rpcs.blocks_by_number.get(&block_num).await
                        {
                            (BlockNumAndHash(block_num, block_hash).into(), changed)
                        } else {
                            (BlockNumOrHash::Num(block_num), changed)
                        }
                    } else {
                        (BlockNumOrHash::Num(block_num), changed)
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

#[derive(Debug, From, Hash, Eq, PartialEq)]
pub enum BlockNumOrHash {
    Num(U64),
    And(BlockNumAndHash),
}

impl BlockNumOrHash {
    pub fn num(&self) -> U64 {
        match self {
            Self::Num(x) => *x,
            Self::And(x) => x.num(),
        }
    }
}

impl From<&Web3ProxyBlock> for BlockNumOrHash {
    fn from(value: &Web3ProxyBlock) -> Self {
        Self::And(value.into())
    }
}

/// TODO: change this to also return the hash needed?
/// this replaces any "latest" identifiers in the JsonRpcRequest with the current block number which feels like the data is structured wrong
#[derive(Debug, Default, Hash, Eq, PartialEq)]
pub enum CacheMode {
    SuccessForever,
    Standard {
        block_needed: BlockNumOrHash,
        cache_block: BlockNumAndHash,
        /// cache jsonrpc errors (server errors are never cached)
        cache_errors: bool,
    },
    Range {
        from_block: BlockNumOrHash,
        to_block: BlockNumOrHash,
        cache_block: BlockNumAndHash,
        /// cache jsonrpc errors (server errors are never cached)
        cache_errors: bool,
    },
    #[default]
    Never,
}

/// TODO: i don't like this. we should make an enum with all of these methods and their types
/// TODO: serde tagged enums should work since the tag is the method
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
        "eth_getBlockByNumber" => Some(0),
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
    pub async fn new<'a>(
        request: &'a mut SingleRequest,
        head_block: Option<&'a Web3ProxyBlock>,
        app: Option<&'a App>,
    ) -> Self {
        match Self::try_new(request, head_block, app).await {
            Ok(x) => return x,
            Err(Web3ProxyError::NoBlocksKnown) => {
                warn!(
                    method = %request.method,
                    params = ?request.params,
                    "no servers available to get block from params"
                );
            }
            Err(err) => {
                error!(
                    method = %request.method,
                    params = ?request.params,
                    ?err,
                    "could not get block from params"
                );
            }
        }

        if let Some(head_block) = head_block {
            Self::Standard {
                block_needed: head_block.into(),
                cache_block: head_block.into(),
                cache_errors: true,
            }
        } else {
            Self::Never
        }
    }

    pub async fn try_new(
        request: &mut SingleRequest,
        head_block: Option<&Web3ProxyBlock>,
        app: Option<&App>,
    ) -> Web3ProxyResult<Self> {
        let params = &mut request.params;

        if head_block.is_none() {
            // since we don't have a head block, i don't trust our anything enough to cache
            return Ok(Self::Never);
        }

        let head_block = head_block.expect("head_block was just checked above");

        if matches!(params, serde_json::Value::Null) {
            // no params given. cache with the head block
            return Ok(Self::Standard {
                block_needed: head_block.into(),
                cache_block: head_block.into(),
                cache_errors: true,
            });
        }

        if let Some(params) = params.as_array() {
            if params.is_empty() {
                // no params given. cache with the head block
                return Ok(Self::Standard {
                    block_needed: head_block.into(),
                    cache_block: head_block.into(),
                    cache_errors: true,
                });
            }
        }

        match request.method.as_ref() {
            "debug_traceTransaction" => {
                // TODO: make sure re-orgs work properly!
                Ok(Self::SuccessForever)
            }
            "eth_gasPrice" => Ok(Self::Never),
            "eth_getBlockByHash" => {
                // TODO: double check that any node can serve this
                // TODO: can a block change? like what if it gets orphaned?
                // TODO: make sure re-orgs work properly!
                Ok(Self::SuccessForever)
            }
            "eth_getBlockTransactionCountByHash" => {
                // TODO: double check that any node can serve this
                Ok(Self::SuccessForever)
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
                    Ok(Self::SuccessForever)
                } else {
                    let from_block = if let Some(x) = obj.get_mut("fromBlock") {
                        // TODO: use .take instead of clone
                        // what if its a hash?
                        let block_num: BlockNumber = serde_json::from_value(x.clone())?;

                        let (block_num, change) =
                            BlockNumber_to_U64(block_num, head_block.number());

                        // TODO: double check this. it scares me
                        // if change {
                        //     // TODO: include the hash instead of the number?
                        //     trace!("changing fromBlock in eth_getLogs. {} -> {}", x, block_num);
                        //     *x = json!(block_num);
                        // }

                        BlockNumOrHash::Num(block_num)
                    } else {
                        BlockNumOrHash::Num(U64::zero())
                    };

                    let to_block = if let Some(x) = obj.get_mut("toBlock") {
                        // TODO: use .take instead of clone
                        // what if its a hash?
                        let block_num: BlockNumber = serde_json::from_value(x.clone())?;

                        // sometimes people request `from_block=head+1, to_block="latest"`. latest becomes head and then theres a problem
                        // TODO: if this is in the future, this cache key won't be very likely to be used again
                        // TODO: delay here until the app has this block?
                        let latest_block = head_block.number().max(from_block.num());

                        let (block_num, change) = BlockNumber_to_U64(block_num, latest_block);

                        // TODO: double check this. it scares me but i think we need it
                        // if change {
                        //     trace!("changing toBlock in eth_getLogs. {} -> {}", x, block_num);
                        //     *x = json!(block_num);
                        // }

                        if let Some(app) = app {
                            // TODO: make a jsonrpc query here? cache rates will be better but it adds a network request
                            if let Some(block_hash) =
                                app.balanced_rpcs.blocks_by_number.get(&block_num).await
                            {
                                BlockNumOrHash::And(BlockNumAndHash(block_num, block_hash))
                            } else {
                                BlockNumOrHash::Num(block_num)
                            }
                        } else {
                            BlockNumOrHash::Num(block_num)
                        }
                    } else {
                        BlockNumOrHash::And(head_block.into())
                    };

                    let cache_block = if let BlockNumOrHash::And(x) = &to_block {
                        x.clone()
                    } else {
                        BlockNumAndHash::from(head_block)
                    };

                    Ok(Self::Range {
                        from_block,
                        to_block,
                        cache_block,
                        cache_errors: true,
                    })
                }
            }
            "eth_getTransactionByBlockHashAndIndex" => {
                // TODO: check a Cache of recent hashes
                // try full nodes first. retry will use archive
                Ok(Self::SuccessForever)
            }
            "eth_getTransactionByHash" => Ok(Self::Never),
            "eth_getTransactionReceipt" => Ok(Self::Never),
            "eth_getUncleByBlockHashAndIndex" => {
                // TODO: check a Cache of recent hashes
                // try full nodes first. retry will use archive
                // TODO: what happens if this block is uncled later?
                Ok(Self::SuccessForever)
            }
            "eth_getUncleCountByBlockHash" => {
                // TODO: check a Cache of recent hashes
                // try full nodes first. retry will use archive
                // TODO: what happens if this block is uncled later?
                Ok(Self::SuccessForever)
            }
            "eth_maxPriorityFeePerGas" => {
                // TODO: this might be too aggressive. i think it can change before a block is mined
                Ok(Self::Never)
            }
            "eth_sendRawTransaction" => Ok(Self::Never),
            "net_listening" => Ok(Self::SuccessForever),
            "net_version" => Ok(Self::SuccessForever),
            method => match get_block_param_id(method) {
                Some(block_param_id) => {
                    let block_needed =
                        clean_block_number(params, block_param_id, head_block, app).await?;

                    let cache_block = match &block_needed {
                        BlockNumOrHash::And(block) => block.clone(),
                        BlockNumOrHash::Num(_) => head_block.into(),
                    };

                    Ok(Self::Standard {
                        block_needed,
                        cache_block,
                        cache_errors: true,
                    })
                }
                None => Err(Web3ProxyError::UnhandledMethod(method.to_string().into())),
            },
        }
    }

    #[inline]
    pub fn cache_jsonrpc_errors(&self) -> bool {
        match self {
            Self::Never => false,
            Self::SuccessForever => true,
            Self::Standard { cache_errors, .. } => *cache_errors,
            Self::Range { cache_errors, .. } => *cache_errors,
        }
    }

    #[inline]
    pub fn from_block(&self) -> Option<&BlockNumOrHash> {
        match self {
            Self::SuccessForever => None,
            Self::Never => None,
            Self::Standard { .. } => None,
            Self::Range { from_block, .. } => Some(from_block),
        }
    }

    #[inline]
    pub fn is_some(&self) -> bool {
        !matches!(self, Self::Never)
    }

    #[inline]
    pub fn to_block(&self) -> Option<&BlockNumOrHash> {
        match self {
            Self::SuccessForever => None,
            Self::Never => None,
            Self::Standard {
                block_needed: block,
                ..
            } => Some(block),
            Self::Range { to_block, .. } => Some(to_block),
        }
    }

    /// get the to_block used **for caching**. This may be the to_block in the request, or it might be the current head block.
    #[inline]
    pub fn cache_block(&self) -> Option<&BlockNumAndHash> {
        match self {
            Self::SuccessForever => None,
            Self::Never => None,
            Self::Standard { cache_block, .. } => Some(cache_block),
            Self::Range { cache_block, .. } => Some(cache_block),
        }
    }
}

#[cfg(test)]
mod test {
    use super::CacheMode;
    use crate::{
        errors::Web3ProxyError,
        jsonrpc::{LooseId, SingleRequest},
        rpcs::blockchain::Web3ProxyBlock,
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

        let id = LooseId::Number(9);

        let mut request = SingleRequest::new(id, method.into(), params).unwrap();

        // TODO: instead of empty, check None?
        let x = CacheMode::try_new(&mut request, Some(&head_block), None)
            .await
            .unwrap();

        assert_eq!(
            x,
            CacheMode::Standard {
                block_needed: (&head_block).into(),
                cache_block: (&head_block).into(),
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

        let id = LooseId::Number(99);

        let mut request = SingleRequest::new(id, method.into(), params).unwrap();

        let x = CacheMode::try_new(&mut request, Some(&head_block), None)
            .await
            .unwrap();

        // "latest" should have been changed to the block number
        assert_eq!(request.params.get(1), Some(&json!(head_block.number())));

        assert_eq!(
            x,
            CacheMode::Standard {
                block_needed: (&head_block).into(),
                cache_block: (&head_block).into(),
                cache_errors: true
            }
        );
    }

    #[test_log::test(tokio::test)]
    async fn test_eth_call_future() {
        let method = "eth_call";

        let head_block_num = 18173997u64;
        let future_block_num = head_block_num + 1;

        let params = json!([{"data": "0xdeadbeef", "to": "0x0000000000000000000000000000000000000000"}, future_block_num]);

        let head_block: Block<H256> = Block {
            number: Some(head_block_num.into()),
            hash: Some(H256::random()),
            ..Default::default()
        };

        let head_block = Web3ProxyBlock::try_new(Arc::new(head_block)).unwrap();

        let mut request = SingleRequest::new(99.into(), method.into(), params).unwrap();

        let x = CacheMode::try_new(&mut request, Some(&head_block), None)
            .await
            .unwrap_err();

        // future blocks should get an error
        match x {
            Web3ProxyError::UnknownBlockNumber { known, unknown } => {
                assert_eq!(known.as_u64(), head_block_num);
                assert_eq!(unknown.as_u64(), future_block_num);
            }
            x => panic!("{:?}", x),
        }

        let x = CacheMode::new(&mut request, Some(&head_block), None).await;

        // TODO: cache with the head block instead?
        matches!(x, CacheMode::Never);
    }

    // TODO: tests for eth_getLogs
}
