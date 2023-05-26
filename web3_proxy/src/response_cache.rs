use crate::{
    frontend::errors::{Web3ProxyError, Web3ProxyResult},
    jsonrpc::JsonRpcErrorData,
    rpcs::blockchain::ArcBlock,
};
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
}

impl JsonRpcQueryCacheKey {
    pub fn from_block_num(&self) -> Option<U64> {
        self.from_block_num
    }
    pub fn to_block_num(&self) -> Option<U64> {
        self.to_block_num
    }
}

impl PartialEq for JsonRpcQueryCacheKey {
    fn eq(&self, other: &Self) -> bool {
        self.hash.eq(&other.hash)
    }
    fn ne(&self, other: &Self) -> bool {
        self.hash.ne(&other.hash)
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
        }
    }
}

pub type JsonRpcQueryCache = CacheWithTTL<
    JsonRpcQueryCacheKey,
    JsonRpcResponseEnum<Box<RawValue>>,
    JsonRpcQueryWeigher,
    DefaultHashBuilder,
>;

#[derive(Clone)]
pub struct JsonRpcQueryWeigher;

/// TODO: we might need one that holds RawValue and one that holds serde_json::Value
#[derive(Clone, Debug)]
pub enum JsonRpcResponseEnum<R> {
    Result {
        value: R,
        size: Option<NonZeroU32>,
    },
    RpcError {
        value: JsonRpcErrorData,
        size: Option<NonZeroU32>,
    },
}

// TODO: impl for other inner result types?
impl JsonRpcResponseEnum<Box<RawValue>> {
    pub fn num_bytes(&self) -> NonZeroU32 {
        // TODO: dry this somehow
        match self {
            JsonRpcResponseEnum::Result { value, size } => size.unwrap_or_else(|| {
                let size = value.get().len();

                NonZeroU32::new(size.clamp(1, u32::MAX as usize) as u32).unwrap()
            }),
            JsonRpcResponseEnum::RpcError { value, size } => size.unwrap_or_else(|| {
                let size = serde_json::to_string(value).unwrap().len();

                NonZeroU32::new(size.clamp(1, u32::MAX as usize) as u32).unwrap()
            }),
        }
    }
}

impl From<serde_json::Value> for JsonRpcResponseEnum<Box<RawValue>> {
    fn from(value: serde_json::Value) -> Self {
        let value = RawValue::from_string(value.to_string()).unwrap();

        Self::Result { value, size: None }
    }
}

impl From<Box<RawValue>> for JsonRpcResponseEnum<Box<RawValue>> {
    fn from(value: Box<RawValue>) -> Self {
        Self::Result { value, size: None }
    }
}

impl<R> From<JsonRpcErrorData> for JsonRpcResponseEnum<R> {
    fn from(value: JsonRpcErrorData) -> Self {
        Self::RpcError { value, size: None }
    }
}

impl<R> TryFrom<Web3ProxyResult<R>> for JsonRpcResponseEnum<R> {
    type Error = Web3ProxyError;

    fn try_from(x: Web3ProxyResult<R>) -> Result<Self, Self::Error> {
        match x {
            Ok(x) => todo!(),
            Err(err) => Err(err),
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

/// TODO: generic type for result once num_bytes works for more things
impl Weighter<JsonRpcQueryCacheKey, (), JsonRpcResponseEnum<Box<RawValue>>>
    for JsonRpcQueryWeigher
{
    fn weight(
        &self,
        _key: &JsonRpcQueryCacheKey,
        _qey: &(),
        value: &JsonRpcResponseEnum<Box<RawValue>>,
    ) -> NonZeroU32 {
        value.num_bytes()
    }
}
