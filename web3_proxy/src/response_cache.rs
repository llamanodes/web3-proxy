use crate::{errors::Web3ProxyError, jsonrpc::JsonRpcErrorData, rpcs::blockchain::ArcBlock};
use derive_more::From;
use ethers::{providers::ProviderError, types::U64};
use hashbrown::hash_map::DefaultHashBuilder;
use moka::future::Cache;
use serde_json::value::RawValue;
use std::{
    hash::{BuildHasher, Hash, Hasher},
    sync::Arc,
};

#[derive(Clone, Debug, Eq, From)]
pub struct JsonRpcQueryCacheKey {
    hash: u64,
    from_block_num: Option<U64>,
    to_block_num: Option<U64>,
    cache_errors: bool,
}

impl JsonRpcQueryCacheKey {
    pub fn hash(&self) -> u64 {
        self.hash
    }
    pub fn from_block_num(&self) -> Option<U64> {
        self.from_block_num
    }
    pub fn to_block_num(&self) -> Option<U64> {
        self.to_block_num
    }
    pub fn cache_errors(&self) -> bool {
        self.cache_errors
    }
}

impl PartialEq for JsonRpcQueryCacheKey {
    fn eq(&self, other: &Self) -> bool {
        self.hash.eq(&other.hash)
    }
}

impl Hash for JsonRpcQueryCacheKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // TODO: i feel like this hashes twice. oh well
        self.hash.hash(state);
    }
}

impl JsonRpcQueryCacheKey {
    pub fn new(
        from_block: Option<ArcBlock>,
        to_block: Option<ArcBlock>,
        method: &str,
        params: &serde_json::Value,
        cache_errors: bool,
    ) -> Self {
        let from_block_num = from_block.as_ref().and_then(|x| x.number);
        let to_block_num = to_block.as_ref().and_then(|x| x.number);

        let mut hasher = DefaultHashBuilder::default().build_hasher();

        from_block.as_ref().and_then(|x| x.hash).hash(&mut hasher);
        to_block.as_ref().and_then(|x| x.hash).hash(&mut hasher);

        method.hash(&mut hasher);

        // TODO: make sure preserve_order feature is OFF
        // TODO: is there a faster way to do this?
        params.to_string().hash(&mut hasher);

        cache_errors.hash(&mut hasher);

        let hash = hasher.finish();

        Self {
            hash,
            from_block_num,
            to_block_num,
            cache_errors,
        }
    }
}

pub type JsonRpcResponseCache = Cache<u64, JsonRpcResponseEnum<Arc<RawValue>>>;

/// TODO: we might need one that holds RawValue and one that holds serde_json::Value
#[derive(Clone, Debug)]
pub enum JsonRpcResponseEnum<R> {
    Result {
        value: R,
        num_bytes: u32,
    },
    RpcError {
        error_data: JsonRpcErrorData,
        num_bytes: u32,
    },
}

// TODO: impl for other inner result types?
impl<R> JsonRpcResponseEnum<R> {
    pub fn num_bytes(&self) -> u32 {
        match self {
            Self::Result { num_bytes, .. } => *num_bytes,
            Self::RpcError { num_bytes, .. } => *num_bytes,
        }
    }
}

impl From<serde_json::Value> for JsonRpcResponseEnum<Arc<RawValue>> {
    fn from(value: serde_json::Value) -> Self {
        let value = RawValue::from_string(value.to_string()).unwrap();

        value.into()
    }
}

impl From<Arc<RawValue>> for JsonRpcResponseEnum<Arc<RawValue>> {
    fn from(value: Arc<RawValue>) -> Self {
        let num_bytes = value.get().len();

        let num_bytes = num_bytes as u32;

        Self::Result { value, num_bytes }
    }
}

impl From<Box<RawValue>> for JsonRpcResponseEnum<Arc<RawValue>> {
    fn from(value: Box<RawValue>) -> Self {
        let num_bytes = value.get().len();

        let num_bytes = num_bytes as u32;

        let value = value.into();

        Self::Result { value, num_bytes }
    }
}

impl<R> TryFrom<Web3ProxyError> for JsonRpcResponseEnum<R> {
    type Error = Web3ProxyError;

    fn try_from(value: Web3ProxyError) -> Result<Self, Self::Error> {
        match value {
            Web3ProxyError::EthersProvider(provider_err) => {
                let err = JsonRpcErrorData::try_from(provider_err)?;

                Ok(err.into())
            }
            err => Err(err),
        }
    }
}

impl TryFrom<Result<Arc<RawValue>, Web3ProxyError>> for JsonRpcResponseEnum<Arc<RawValue>> {
    type Error = Web3ProxyError;

    fn try_from(value: Result<Arc<RawValue>, Web3ProxyError>) -> Result<Self, Self::Error> {
        match value {
            Ok(x) => Ok(x.into()),
            Err(err) => {
                let x: Self = err.try_into()?;

                Ok(x)
            }
        }
    }
}
impl TryFrom<Result<Box<RawValue>, Web3ProxyError>> for JsonRpcResponseEnum<Arc<RawValue>> {
    type Error = Web3ProxyError;

    fn try_from(value: Result<Box<RawValue>, Web3ProxyError>) -> Result<Self, Self::Error> {
        match value {
            Ok(x) => Ok(x.into()),
            Err(err) => {
                let x: Self = err.try_into()?;

                Ok(x)
            }
        }
    }
}

impl<R> From<JsonRpcErrorData> for JsonRpcResponseEnum<R> {
    fn from(value: JsonRpcErrorData) -> Self {
        // TODO: wrap the error in a complete response?
        let num_bytes = serde_json::to_string(&value).unwrap().len();

        let num_bytes = num_bytes as u32;

        Self::RpcError {
            error_data: value,
            num_bytes,
        }
    }
}

impl TryFrom<ProviderError> for JsonRpcErrorData {
    type Error = Web3ProxyError;

    fn try_from(e: ProviderError) -> Result<Self, Self::Error> {
        // TODO: move turning ClientError into json to a helper function?
        let code;
        let message: String;
        let data;

        match e {
            ProviderError::JsonRpcClientError(err) => {
                if let Some(err) = err.as_error_response() {
                    code = err.code;
                    message = err.message.clone();
                    data = err.data.clone();
                } else if let Some(err) = err.as_serde_error() {
                    // this is not an rpc error. keep it as an error
                    return Err(Web3ProxyError::BadResponse(err.to_string().into()));
                } else {
                    return Err(anyhow::anyhow!("unexpected ethers error! {:?}", err).into());
                }
            }
            e => return Err(e.into()),
        }

        Ok(JsonRpcErrorData {
            code,
            message: message.into(),
            data,
        })
    }
}

pub fn json_rpc_response_weigher<K, R>(_key: &K, value: &JsonRpcResponseEnum<R>) -> u32 {
    value.num_bytes()
}

#[cfg(test)]
mod tests {
    use super::JsonRpcResponseEnum;
    use crate::response_cache::json_rpc_response_weigher;
    use serde_json::value::RawValue;
    use std::sync::Arc;

    #[tokio::test(start_paused = true)]
    async fn test_json_rpc_query_weigher() {
        let max_item_weight = 200;
        let weight_capacity = 1_000;

        // let test_cache: Cache<u32, JsonRpcResponseEnum<Arc<RawValue>>> =
        //     CacheBuilder::new(weight_capacity)
        //         .weigher(json_rpc_response_weigher)
        //         .time_to_live(Duration::from_secs(2))
        //         .build();

        let small_data: JsonRpcResponseEnum<Arc<RawValue>> = JsonRpcResponseEnum::Result {
            value: Box::<RawValue>::default().into(),
            num_bytes: max_item_weight / 2,
        };

        assert_eq!(
            json_rpc_response_weigher(&(), &small_data),
            max_item_weight / 2
        );

        let max_sized_data: JsonRpcResponseEnum<Arc<RawValue>> = JsonRpcResponseEnum::Result {
            value: Box::<RawValue>::default().into(),
            num_bytes: max_item_weight,
        };

        assert_eq!(
            json_rpc_response_weigher(&(), &max_sized_data),
            max_item_weight
        );

        let oversized_data: JsonRpcResponseEnum<Arc<RawValue>> = JsonRpcResponseEnum::Result {
            value: Box::<RawValue>::default().into(),
            num_bytes: max_item_weight * 2,
        };

        assert_eq!(
            json_rpc_response_weigher(&(), &oversized_data),
            max_item_weight * 2
        );

        // TODO: helper for inserts that does size checking
        /*
        test_cache.insert(0, small_data).await;

        test_cache.get(&0).unwrap();

        test_cache.insert(1, max_sized_data).await;

        test_cache.get(&0).unwrap();
        test_cache.get(&1).unwrap();

        // TODO: this will currently work! need to wrap moka cache in a checked insert
        test_cache.insert(2, oversized_data).await;

        test_cache.get(&0).unwrap();
        test_cache.get(&1).unwrap();
        assert!(test_cache.get(&2).is_none());
        */
    }
}
