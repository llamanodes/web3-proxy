use crate::{errors::Web3ProxyError, jsonrpc::JsonRpcErrorData, rpcs::blockchain::ArcBlock};
use derive_more::From;
use ethers::{providers::ProviderError, types::U64};
use hashbrown::hash_map::DefaultHashBuilder;
use quick_cache_ttl::{CacheWithTTL, Weighter};
use serde_json::value::RawValue;
use std::{
    borrow::Cow,
    hash::{BuildHasher, Hash, Hasher},
    num::NonZeroU32,
};

#[derive(Clone, Debug, Eq, From)]
pub struct JsonRpcQueryCacheKey {
    hash: u64,
    from_block_num: Option<U64>,
    to_block_num: Option<U64>,
    cache_errors: bool,
}

impl JsonRpcQueryCacheKey {
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

pub type JsonRpcResponseCache =
    CacheWithTTL<JsonRpcQueryCacheKey, JsonRpcResponseEnum<Box<RawValue>>, JsonRpcResponseWeigher>;

#[derive(Clone)]
pub struct JsonRpcResponseWeigher;

/// TODO: we might need one that holds RawValue and one that holds serde_json::Value
#[derive(Clone, Debug)]
pub enum JsonRpcResponseEnum<R> {
    Result {
        value: R,
        num_bytes: NonZeroU32,
    },
    RpcError {
        error_data: JsonRpcErrorData,
        num_bytes: NonZeroU32,
    },
}

// TODO: impl for other inner result types?
impl<R> JsonRpcResponseEnum<R> {
    pub fn num_bytes(&self) -> NonZeroU32 {
        match self {
            Self::Result { num_bytes, .. } => *num_bytes,
            Self::RpcError { num_bytes, .. } => *num_bytes,
        }
    }
}

impl From<serde_json::Value> for JsonRpcResponseEnum<Box<RawValue>> {
    fn from(value: serde_json::Value) -> Self {
        let value = RawValue::from_string(value.to_string()).unwrap();

        value.into()
    }
}

impl From<Box<RawValue>> for JsonRpcResponseEnum<Box<RawValue>> {
    fn from(value: Box<RawValue>) -> Self {
        let num_bytes = value.get().len();

        let num_bytes = NonZeroU32::try_from(num_bytes as u32).unwrap();

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

impl TryFrom<Result<Box<RawValue>, Web3ProxyError>> for JsonRpcResponseEnum<Box<RawValue>> {
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

        let num_bytes = NonZeroU32::try_from(num_bytes as u32).unwrap();

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
                    return Err(Web3ProxyError::BadResponse(format!(
                        "bad response: {}",
                        err
                    )));
                } else {
                    return Err(anyhow::anyhow!("unexpected ethers error! {:?}", err).into());
                }
            }
            e => return Err(e.into()),
        }

        Ok(JsonRpcErrorData {
            code,
            message: Cow::Owned(message),
            data,
        })
    }
}

impl<K, Q> Weighter<K, Q, JsonRpcResponseEnum<Box<RawValue>>> for JsonRpcResponseWeigher {
    fn weight(&self, _key: &K, _qey: &Q, value: &JsonRpcResponseEnum<Box<RawValue>>) -> NonZeroU32 {
        value.num_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::{JsonRpcResponseEnum, JsonRpcResponseWeigher};
    use quick_cache_ttl::CacheWithTTL;
    use serde_json::value::RawValue;
    use std::{num::NonZeroU32, time::Duration};

    #[tokio::test(start_paused = true)]
    async fn test_json_rpc_query_weigher() {
        let max_item_weight = 200;
        let weight_capacity = 1_000;

        let test_cache: CacheWithTTL<
            u32,
            JsonRpcResponseEnum<Box<RawValue>>,
            JsonRpcResponseWeigher,
        > = CacheWithTTL::new_with_weights(
            "test",
            5,
            max_item_weight.try_into().unwrap(),
            weight_capacity,
            JsonRpcResponseWeigher,
            Duration::from_secs(2),
        )
        .await;

        let small_data = JsonRpcResponseEnum::Result {
            value: Default::default(),
            num_bytes: NonZeroU32::try_from(max_item_weight / 2).unwrap(),
        };

        let max_sized_data = JsonRpcResponseEnum::Result {
            value: Default::default(),
            num_bytes: NonZeroU32::try_from(max_item_weight).unwrap(),
        };

        let oversized_data = JsonRpcResponseEnum::Result {
            value: Default::default(),
            num_bytes: NonZeroU32::try_from(max_item_weight * 2).unwrap(),
        };

        test_cache.try_insert(0, small_data).unwrap();

        test_cache.get(&0).unwrap();

        test_cache.try_insert(1, max_sized_data).unwrap();

        test_cache.get(&0).unwrap();
        test_cache.get(&1).unwrap();

        test_cache.try_insert(2, oversized_data).unwrap_err();

        test_cache.get(&0).unwrap();
        test_cache.get(&1).unwrap();
        assert!(test_cache.get(&2).is_none());
    }
}
