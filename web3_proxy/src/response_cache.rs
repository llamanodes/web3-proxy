use crate::{block_number::BlockNumAndHash, errors::Web3ProxyError, jsonrpc::JsonRpcErrorData};
use derive_more::From;
use ethers::{
    providers::{HttpClientError, JsonRpcError, ProviderError, WsClientError},
    types::U64,
};
use hashbrown::hash_map::DefaultHashBuilder;
use moka::future::Cache;
use serde_json::value::RawValue;
use std::{
    hash::{BuildHasher, Hash, Hasher},
    sync::Arc,
};

#[derive(Clone, Debug, Eq, From)]
pub struct JsonRpcQueryCacheKey {
    /// hashed inputs
    hash: u64,
    from_block: Option<BlockNumAndHash>,
    to_block: Option<BlockNumAndHash>,
    cache_errors: bool,
}

impl JsonRpcQueryCacheKey {
    pub fn hash(&self) -> u64 {
        self.hash
    }
    pub fn from_block_num(&self) -> Option<&U64> {
        self.from_block.as_ref().map(|x| x.num())
    }
    pub fn to_block_num(&self) -> Option<&U64> {
        self.to_block.as_ref().map(|x| x.num())
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
        from_block: Option<BlockNumAndHash>,
        to_block: Option<BlockNumAndHash>,
        method: &str,
        params: &serde_json::Value,
        cache_errors: bool,
    ) -> Self {
        let from_block_hash = from_block.as_ref().map(|x| x.hash());
        let to_block_hash = to_block.as_ref().map(|x| x.hash());

        let mut hasher = DefaultHashBuilder::default().build_hasher();

        from_block_hash.hash(&mut hasher);
        to_block_hash.hash(&mut hasher);

        method.hash(&mut hasher);

        // TODO: make sure preserve_order feature is OFF
        // TODO: is there a faster way to do this?
        params.to_string().hash(&mut hasher);

        cache_errors.hash(&mut hasher);

        let hash = hasher.finish();

        Self {
            hash,
            from_block,
            to_block,
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
        if let Web3ProxyError::EthersProvider(ref err) = value {
            if let Ok(x) = JsonRpcErrorData::try_from(err) {
                let x = x.into();

                return Ok(x);
            }
        }

        Err(value)
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

impl<'a> From<&'a JsonRpcError> for JsonRpcErrorData {
    fn from(value: &'a JsonRpcError) -> Self {
        Self {
            code: value.code,
            message: value.message.clone().into(),
            data: value.data.clone(),
        }
    }
}

impl<'a> TryFrom<&'a ProviderError> for JsonRpcErrorData {
    type Error = &'a ProviderError;

    fn try_from(e: &'a ProviderError) -> Result<Self, Self::Error> {
        match e {
            ProviderError::JsonRpcClientError(err) => {
                if let Some(err) = err.as_error_response() {
                    Ok(err.into())
                } else {
                    Err(e)
                }
            }
            e => Err(e),
        }
    }
}

impl<'a> TryFrom<&'a HttpClientError> for JsonRpcErrorData {
    type Error = &'a HttpClientError;

    fn try_from(e: &'a HttpClientError) -> Result<Self, Self::Error> {
        match e {
            HttpClientError::JsonRpcError(err) => Ok(err.into()),
            e => Err(e),
        }
    }
}

impl<'a> TryFrom<&'a WsClientError> for JsonRpcErrorData {
    type Error = &'a WsClientError;

    fn try_from(e: &'a WsClientError) -> Result<Self, Self::Error> {
        match e {
            WsClientError::JsonRpcError(err) => Ok(err.into()),
            e => Err(e),
        }
    }
}

/// The inner u32 is the maximum weight per item
#[derive(Copy, Clone)]
pub struct JsonRpcResponseWeigher(pub u32);

impl JsonRpcResponseWeigher {
    pub fn weigh<K, R>(&self, _key: &K, value: &JsonRpcResponseEnum<R>) -> u32 {
        let x = value.num_bytes();

        if x > self.0 {
            // return max. the item may start to be inserted into the cache, but it will be immediatly removed
            u32::MAX
        } else {
            x
        }
    }
}

#[cfg(test)]
mod tests {
    use super::JsonRpcResponseEnum;
    use crate::response_cache::JsonRpcResponseWeigher;
    use moka::future::{Cache, CacheBuilder, ConcurrentCacheExt};
    use serde_json::value::RawValue;
    use std::{sync::Arc, time::Duration};

    #[tokio::test(start_paused = true)]
    async fn test_json_rpc_query_weigher() {
        let max_item_weight = 200;
        let weight_capacity = 1_000;

        let weigher = JsonRpcResponseWeigher(max_item_weight);

        let small_data: JsonRpcResponseEnum<Arc<RawValue>> = JsonRpcResponseEnum::Result {
            value: Box::<RawValue>::default().into(),
            num_bytes: max_item_weight / 2,
        };

        assert_eq!(weigher.weigh(&(), &small_data), max_item_weight / 2);

        let max_sized_data: JsonRpcResponseEnum<Arc<RawValue>> = JsonRpcResponseEnum::Result {
            value: Box::<RawValue>::default().into(),
            num_bytes: max_item_weight,
        };

        assert_eq!(weigher.weigh(&(), &max_sized_data), max_item_weight);

        let oversized_data: JsonRpcResponseEnum<Arc<RawValue>> = JsonRpcResponseEnum::Result {
            value: Box::<RawValue>::default().into(),
            num_bytes: max_item_weight * 2,
        };

        assert_eq!(weigher.weigh(&(), &oversized_data), u32::MAX);

        let test_cache: Cache<u32, JsonRpcResponseEnum<Arc<RawValue>>> =
            CacheBuilder::new(weight_capacity)
                .weigher(move |k, v| weigher.weigh(k, v))
                .time_to_live(Duration::from_secs(2))
                .build();

        test_cache.insert(0, small_data).await;

        test_cache.get(&0).unwrap();

        test_cache.insert(1, max_sized_data).await;

        test_cache.get(&0).unwrap();
        test_cache.get(&1).unwrap();

        test_cache.insert(2, oversized_data).await;

        test_cache.get(&0).unwrap();
        test_cache.get(&1).unwrap();

        // oversized data will be in the cache temporarily (it should just be an arc though, so that should be fine)
        test_cache.get(&2).unwrap();

        // sync should do necessary cleanup
        test_cache.sync();

        // now it should be empty
        assert!(test_cache.get(&2).is_none());
    }
}
