use crate::{
    frontend::errors::Web3ProxyError, jsonrpc::JsonRpcErrorData, rpcs::blockchain::ArcBlock,
};
use derive_more::From;
use ethers::providers::ProviderError;
use quick_cache_ttl::{CacheWithTTL, Weighter};
use serde_json::value::RawValue;
use std::{
    borrow::Cow,
    hash::{Hash, Hasher},
    num::NonZeroU32,
};

#[derive(Clone, Debug, From, PartialEq, Eq)]
pub struct JsonRpcResponseCacheKey {
    pub from_block: Option<ArcBlock>,
    pub to_block: Option<ArcBlock>,
    pub method: String,
    pub params: Option<serde_json::Value>,
    pub cache_errors: bool,
}

impl Hash for JsonRpcResponseCacheKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.from_block.as_ref().map(|x| x.hash).hash(state);
        self.to_block.as_ref().map(|x| x.hash).hash(state);
        self.method.hash(state);

        // make sure preserve_order feature is OFF
        self.params.as_ref().map(|x| x.to_string()).hash(state);

        self.cache_errors.hash(state)
    }
}

pub type JsonRpcResponseCache =
    CacheWithTTL<JsonRpcResponseCacheKey, JsonRpcResponseData, JsonRpcResponseWeigher>;

#[derive(Clone)]
pub struct JsonRpcResponseWeigher;

#[derive(Clone, Debug)]
pub enum JsonRpcResponseData {
    Result {
        value: Box<RawValue>,
        num_bytes: NonZeroU32,
    },
    Error {
        value: JsonRpcErrorData,
        num_bytes: NonZeroU32,
    },
}

impl JsonRpcResponseData {
    pub fn num_bytes(&self) -> NonZeroU32 {
        // TODO: dry this somehow
        match self {
            JsonRpcResponseData::Result { num_bytes, .. } => *num_bytes,
            JsonRpcResponseData::Error { num_bytes, .. } => *num_bytes,
        }
    }
}

impl From<serde_json::Value> for JsonRpcResponseData {
    fn from(value: serde_json::Value) -> Self {
        let value = RawValue::from_string(value.to_string()).unwrap();

        value.into()
    }
}

impl From<Box<RawValue>> for JsonRpcResponseData {
    fn from(value: Box<RawValue>) -> Self {
        let num_bytes = value.get().len();

        let num_bytes = NonZeroU32::try_from(num_bytes as u32).unwrap();

        Self::Result { value, num_bytes }
    }
}

impl From<JsonRpcErrorData> for JsonRpcResponseData {
    fn from(value: JsonRpcErrorData) -> Self {
        // TODO: wrap the error in a complete response?
        let num_bytes = serde_json::to_string(&value).unwrap().len();

        let num_bytes = NonZeroU32::try_from(num_bytes as u32).unwrap();

        Self::Error { value, num_bytes }
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

impl<K, Q> Weighter<K, Q, JsonRpcResponseData> for JsonRpcResponseWeigher {
    fn weight(&self, _key: &K, _qey: &Q, value: &JsonRpcResponseData) -> NonZeroU32 {
        value.num_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::{JsonRpcResponseData, JsonRpcResponseWeigher};
    use quick_cache_ttl::CacheWithTTL;
    use std::{num::NonZeroU32, time::Duration};

    #[tokio::test(start_paused = true)]
    async fn test_json_rpc_query_weigher() {
        let max_item_weight = 200;
        let weight_capacity = 1_000;

        let response_cache: CacheWithTTL<u32, JsonRpcResponseData, JsonRpcResponseWeigher> =
            CacheWithTTL::new_with_weights(
                "test",
                5,
                max_item_weight.try_into().unwrap(),
                weight_capacity,
                JsonRpcResponseWeigher,
                Duration::from_secs(2),
            )
            .await;

        let small_data: JsonRpcResponseData = JsonRpcResponseData::Result {
            value: Default::default(),
            num_bytes: NonZeroU32::try_from(max_item_weight / 2).unwrap(),
        };

        let max_sized_data = JsonRpcResponseData::Result {
            value: Default::default(),
            num_bytes: NonZeroU32::try_from(max_item_weight).unwrap(),
        };

        let oversized_data = JsonRpcResponseData::Result {
            value: Default::default(),
            num_bytes: NonZeroU32::try_from(max_item_weight * 2).unwrap(),
        };

        response_cache.try_insert(0, small_data).unwrap();

        response_cache.get(&0).unwrap();

        response_cache.try_insert(1, max_sized_data).unwrap();

        response_cache.get(&0).unwrap();
        response_cache.get(&1).unwrap();

        response_cache.try_insert(2, oversized_data).unwrap_err();

        response_cache.get(&0).unwrap();
        response_cache.get(&1).unwrap();
        assert!(response_cache.get(&2).is_none());
    }
}
