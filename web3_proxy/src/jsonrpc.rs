use axum::body::StreamBody;
use axum::response::{IntoResponse, Response as AxumResponse};
use axum::Json;
use bytes::{Bytes, BytesMut};
use derive_more::From;
use ethers::providers::ProviderError;
use futures_util::stream::{self, StreamExt};
use futures_util::TryStreamExt;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::value::{to_raw_value, RawValue};
use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;
use std::sync::{atomic, Arc};
use std::time::Duration;
use tokio::time::sleep;

use crate::app::Web3ProxyApp;
use crate::errors::{Web3ProxyError, Web3ProxyResult};
use crate::frontend::authorization::{Authorization, RequestMetadata, RequestOrMethod};
use crate::response_cache::JsonRpcResponseEnum;

pub trait JsonRpcParams = fmt::Debug + serde::Serialize + Send + Sync + 'static;
pub trait JsonRpcResultData = serde::Serialize + serde::de::DeserializeOwned + fmt::Debug + Send;

// TODO: borrow values to avoid allocs if possible
#[derive(Debug, Serialize)]
pub struct ParsedResponse<T = Arc<RawValue>> {
    jsonrpc: String,
    id: Option<Box<RawValue>>,
    #[serde(flatten)]
    pub payload: Payload<T>,
}

impl ParsedResponse {
    pub fn from_value(value: serde_json::Value, id: Box<RawValue>) -> Self {
        let result = serde_json::value::to_raw_value(&value)
            .expect("this should not fail")
            .into();
        Self::from_result(result, Some(id))
    }
}

impl ParsedResponse<Arc<RawValue>> {
    pub fn from_response_data(data: JsonRpcResponseEnum<Arc<RawValue>>, id: Box<RawValue>) -> Self {
        match data {
            JsonRpcResponseEnum::NullResult => {
                let x: Box<RawValue> = Default::default();
                Self::from_result(Arc::from(x), Some(id))
            }
            JsonRpcResponseEnum::RpcError { error_data, .. } => Self::from_error(error_data, id),
            JsonRpcResponseEnum::Result { value, .. } => Self::from_result(value, Some(id)),
        }
    }
}

impl<T> ParsedResponse<T> {
    pub fn from_result(result: T, id: Option<Box<RawValue>>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            payload: Payload::Success { result },
        }
    }

    pub fn from_error(error: JsonRpcErrorData, id: Box<RawValue>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: Some(id),
            payload: Payload::Error { error },
        }
    }

    pub fn result(&self) -> Option<&T> {
        match &self.payload {
            Payload::Success { result } => Some(result),
            Payload::Error { .. } => None,
        }
    }

    pub fn into_result(self) -> Web3ProxyResult<T> {
        match self.payload {
            Payload::Success { result } => Ok(result),
            Payload::Error { error } => Err(Web3ProxyError::JsonRpcErrorData(error)),
        }
    }
}

impl<'de, T> Deserialize<'de> for ParsedResponse<T>
where
    T: de::DeserializeOwned,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ResponseVisitor<T>(PhantomData<T>);
        impl<'de, T> de::Visitor<'de> for ResponseVisitor<T>
        where
            T: de::DeserializeOwned,
        {
            type Value = ParsedResponse<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a valid jsonrpc 2.0 response object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: de::MapAccess<'de>,
            {
                let mut jsonrpc = None;

                // response & error
                let mut id = None;
                // only response
                let mut result = None;
                // only error
                let mut error = None;

                while let Some(key) = map.next_key()? {
                    match key {
                        "jsonrpc" => {
                            if jsonrpc.is_some() {
                                return Err(de::Error::duplicate_field("jsonrpc"));
                            }

                            let value = map.next_value()?;
                            if value != "2.0" {
                                return Err(de::Error::invalid_value(
                                    de::Unexpected::Str(value),
                                    &"2.0",
                                ));
                            }

                            jsonrpc = Some(value);
                        }
                        "id" => {
                            if id.is_some() {
                                return Err(de::Error::duplicate_field("id"));
                            }

                            let value: Box<RawValue> = map.next_value()?;
                            id = Some(value);
                        }
                        "result" => {
                            if result.is_some() {
                                return Err(de::Error::duplicate_field("result"));
                            }

                            let value: T = map.next_value()?;
                            result = Some(value);
                        }
                        "error" => {
                            if error.is_some() {
                                return Err(de::Error::duplicate_field("Error"));
                            }

                            let value: JsonRpcErrorData = map.next_value()?;
                            error = Some(value);
                        }
                        key => {
                            return Err(de::Error::unknown_field(
                                key,
                                &["jsonrpc", "id", "result", "error"],
                            ));
                        }
                    }
                }

                // jsonrpc version must be present in all responses
                let jsonrpc = jsonrpc
                    .ok_or_else(|| de::Error::missing_field("jsonrpc"))?
                    .to_string();

                let payload = match (result, error) {
                    (Some(result), None) => Payload::Success { result },
                    (None, Some(error)) => Payload::Error { error },
                    _ => {
                        return Err(de::Error::custom(
                            "response must be either a success or error object",
                        ))
                    }
                };

                Ok(ParsedResponse {
                    jsonrpc,
                    id,
                    payload,
                })
            }
        }

        deserializer.deserialize_map(ResponseVisitor(PhantomData))
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Payload<T> {
    Success { result: T },
    Error { error: JsonRpcErrorData },
}

#[derive(Debug)]
pub struct StreamResponse {
    buffer: Bytes,
    response: reqwest::Response,
    request_metadata: Arc<RequestMetadata>,
}

impl StreamResponse {
    // TODO: error handing
    pub async fn read<T>(self) -> Result<ParsedResponse<T>, ProviderError>
    where
        T: de::DeserializeOwned,
    {
        let mut buffer = BytesMut::with_capacity(self.buffer.len());
        buffer.extend_from_slice(&self.buffer);
        buffer.extend_from_slice(&self.response.bytes().await?);
        let parsed = serde_json::from_slice(&buffer)?;
        Ok(parsed)
    }
}

impl IntoResponse for StreamResponse {
    fn into_response(self) -> axum::response::Response {
        let stream = stream::once(async { Ok::<_, reqwest::Error>(self.buffer) })
            .chain(self.response.bytes_stream())
            .map_ok(move |x| {
                let len = x.len();

                self.request_metadata.add_response(len);

                x
            });
        let body = StreamBody::new(stream);
        body.into_response()
    }
}

#[derive(Debug)]
pub enum SingleResponse<T = Arc<RawValue>> {
    Parsed(ParsedResponse<T>),
    Stream(StreamResponse),
}

impl<T> SingleResponse<T>
where
    T: de::DeserializeOwned + Serialize,
{
    // TODO: threshold from configs
    // TODO: error handling
    pub async fn read_if_short(
        mut response: reqwest::Response,
        nbytes: u64,
        request_metadata: Arc<RequestMetadata>,
    ) -> Result<SingleResponse<T>, ProviderError> {
        match response.content_length() {
            // short
            Some(len) if len <= nbytes => Ok(Self::from_bytes(response.bytes().await?)?),
            // long
            Some(_) => Ok(Self::Stream(StreamResponse {
                buffer: Bytes::new(),
                response,
                request_metadata,
            })),
            None => {
                let mut buffer = BytesMut::new();
                while (buffer.len() as u64) < nbytes {
                    match response.chunk().await? {
                        Some(chunk) => {
                            buffer.extend_from_slice(&chunk);
                        }
                        None => return Ok(Self::from_bytes(buffer.freeze())?),
                    }
                }
                let buffer = buffer.freeze();
                Ok(Self::Stream(StreamResponse {
                    buffer,
                    response,
                    request_metadata,
                }))
            }
        }
    }

    fn from_bytes(buf: Bytes) -> Result<Self, serde_json::Error> {
        let val = serde_json::from_slice(&buf)?;
        Ok(Self::Parsed(val))
    }

    // TODO: error handling
    pub async fn parsed(self) -> Result<ParsedResponse<T>, ProviderError> {
        match self {
            Self::Parsed(resp) => Ok(resp),
            Self::Stream(resp) => resp.read().await,
        }
    }

    pub fn num_bytes(&self) -> usize {
        match self {
            Self::Parsed(response) => serde_json::to_string(response)
                .expect("this should always serialize")
                .len(),
            Self::Stream(response) => match response.response.content_length() {
                Some(len) => len as usize,
                None => 0,
            },
        }
    }
}

impl<T> From<ParsedResponse<T>> for SingleResponse<T> {
    fn from(response: ParsedResponse<T>) -> Self {
        Self::Parsed(response)
    }
}

impl<T> IntoResponse for SingleResponse<T>
where
    T: Serialize,
{
    fn into_response(self) -> axum::response::Response {
        match self {
            Self::Parsed(resp) => Json(resp).into_response(),
            Self::Stream(resp) => resp.into_response(),
        }
    }
}

#[derive(Debug)]
pub enum Response<T = Arc<RawValue>> {
    Single(SingleResponse<T>),
    Batch(Vec<ParsedResponse<T>>),
}

impl<T> Response<T> {
    pub fn to_json_string(&self) -> Result<String, ProviderError> {
        todo!()
    }
}

impl<T> From<ParsedResponse<T>> for Response<T> {
    fn from(response: ParsedResponse<T>) -> Self {
        Self::Single(SingleResponse::Parsed(response))
    }
}

impl<T> IntoResponse for Response<T>
where
    T: Serialize,
{
    fn into_response(self) -> axum::response::Response {
        match self {
            Self::Single(resp) => resp.into_response(),
            Self::Batch(resps) => Json(resps).into_response(),
        }
    }
}

// TODO: &str here instead of String should save a lot of allocations
// TODO: generic type for params?
#[derive(Clone, Deserialize, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    /// id could be a stricter type, but many rpcs do things against the spec
    pub id: Box<RawValue>,
    pub method: String,
    /// TODO: skip serializing if serde_json::Value::Null
    pub params: serde_json::Value,
}

#[derive(From)]
pub enum JsonRpcId {
    None,
    Number(u64),
    String(String),
}

impl JsonRpcId {
    pub fn to_raw_value(self) -> Box<RawValue> {
        // TODO: is this a good way to do this? we should probably use references
        match self {
            Self::None => Default::default(),
            Self::Number(x) => {
                serde_json::from_value(json!(x)).expect("number id should always work")
            }
            Self::String(x) => serde_json::from_str(&x).expect("string id should always work"),
        }
    }
}

impl JsonRpcRequest {
    // TODO: Web3ProxyResult? can this even fail?
    pub fn new(id: JsonRpcId, method: String, params: serde_json::Value) -> anyhow::Result<Self> {
        let x = Self {
            jsonrpc: "2.0".to_string(),
            id: id.to_raw_value(),
            method,
            params,
        };

        Ok(x)
    }

    pub fn validate_method(&self) -> bool {
        self.method
            .chars()
            .all(|x| x.is_ascii_alphanumeric() || x == '_' || x == '(' || x == ')')
    }
}

impl fmt::Debug for JsonRpcRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        // TODO: how should we include params in this? maybe just the length?
        f.debug_struct("JsonRpcRequest")
            .field("id", &self.id)
            .field("method", &self.method)
            .field("params", &self.params)
            .finish()
    }
}

/// Requests can come in multiple formats
#[derive(Debug, From, Serialize)]
pub enum JsonRpcRequestEnum {
    Batch(Vec<JsonRpcRequest>),
    Single(JsonRpcRequest),
}

impl JsonRpcRequestEnum {
    pub fn first_id(&self) -> Option<Box<RawValue>> {
        match self {
            Self::Batch(x) => x.first().map(|x| x.id.clone()),
            Self::Single(x) => Some(x.id.clone()),
        }
    }

    /// returns the id of the first invalid result (if any). None is good
    pub fn validate(&self) -> Option<Box<RawValue>> {
        match self {
            Self::Batch(x) => x
                .iter()
                .find_map(|x| (!x.validate_method()).then_some(x.id.clone())),
            Self::Single(x) => {
                if x.validate_method() {
                    None
                } else {
                    Some(x.id.clone())
                }
            }
        }
    }

    /// returns the id of the first invalid result (if any). None is good
    pub async fn tarpit_invalid(
        &self,
        app: &Web3ProxyApp,
        authorization: &Arc<Authorization>,
        duration: Duration,
    ) -> Result<(), AxumResponse> {
        let err_id = match self.validate() {
            None => return Ok(()),
            Some(x) => x,
        };

        let size = serde_json::to_string(&self)
            .expect("JsonRpcRequestEnum should always serialize")
            .len();

        let request = RequestOrMethod::Method("invalid_method", size);

        // TODO: create a stat so we can penalize
        // TODO: what request size
        let metadata = RequestMetadata::new(app, authorization.clone(), request, None).await;

        metadata
            .user_error_response
            .store(true, atomic::Ordering::Release);

        let response = Web3ProxyError::BadRequest("request failed validation".into());

        metadata.add_response(&response);

        let response = response.into_response_with_id(Some(err_id));

        // TODO: variable duration depending on the IP
        sleep(duration).await;

        let _ = metadata.try_send_arc_stat();

        Err(response)
    }
}

impl<'de> Deserialize<'de> for JsonRpcRequestEnum {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(field_identifier, rename_all = "lowercase")]
        enum Field {
            JsonRpc,
            Id,
            Method,
            Params,
        }

        struct JsonRpcBatchVisitor;

        impl<'de> Visitor<'de> for JsonRpcBatchVisitor {
            type Value = JsonRpcRequestEnum;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("JsonRpcRequestEnum")
            }

            fn visit_seq<V>(self, mut seq: V) -> Result<JsonRpcRequestEnum, V::Error>
            where
                V: SeqAccess<'de>,
            {
                // TODO: what size should we use as the default?
                let mut batch: Vec<JsonRpcRequest> =
                    Vec::with_capacity(seq.size_hint().unwrap_or(10));

                while let Ok(Some(s)) = seq.next_element::<JsonRpcRequest>() {
                    batch.push(s);
                }

                Ok(JsonRpcRequestEnum::Batch(batch))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                // TODO: i feel like this should be easier
                let mut jsonrpc = None;
                let mut id = None;
                let mut method = None;
                let mut params = None;

                while let Some(key) = map.next_key()? {
                    match key {
                        Field::JsonRpc => {
                            // throw away the value
                            // TODO: should we check that it's 2.0?
                            // TODO: how do we skip over this value entirely?
                            jsonrpc = Some(map.next_value()?);
                        }
                        Field::Id => {
                            if id.is_some() {
                                return Err(de::Error::duplicate_field("id"));
                            }
                            id = Some(map.next_value()?);
                        }
                        Field::Method => {
                            if method.is_some() {
                                return Err(de::Error::duplicate_field("method"));
                            }
                            method = Some(map.next_value()?);
                        }
                        Field::Params => {
                            if params.is_some() {
                                return Err(de::Error::duplicate_field("params"));
                            }
                            params = Some(map.next_value()?);
                        }
                    }
                }

                // some providers don't follow the spec and dont include the jsonrpc key
                // i think "2.0" should be a fine default to handle these incompatible clones
                let jsonrpc = jsonrpc.unwrap_or_else(|| "2.0".to_string());
                // TODO: Errors returned by the try operator get shown in an ugly way
                let id = id.ok_or_else(|| de::Error::missing_field("id"))?;
                let method = method.ok_or_else(|| de::Error::missing_field("method"))?;

                let single = JsonRpcRequest {
                    jsonrpc,
                    id,
                    method,
                    params: params.unwrap_or_default(),
                };

                Ok(JsonRpcRequestEnum::Single(single))
            }
        }

        let batch_visitor = JsonRpcBatchVisitor {};

        deserializer.deserialize_any(batch_visitor)
    }
}

// TODO: impl Error on this?
/// All jsonrpc errors use this structure
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct JsonRpcErrorData {
    /// The error code
    pub code: i64,
    /// The error message
    pub message: Cow<'static, str>,
    /// Additional data
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl JsonRpcErrorData {
    pub fn num_bytes(&self) -> usize {
        serde_json::to_string(self)
            .expect("should always serialize")
            .len()
    }
}

impl From<&'static str> for JsonRpcErrorData {
    fn from(value: &'static str) -> Self {
        Self {
            code: -32000,
            message: value.into(),
            data: None,
        }
    }
}

impl From<String> for JsonRpcErrorData {
    fn from(value: String) -> Self {
        Self {
            code: -32000,
            message: value.into(),
            data: None,
        }
    }
}

/// A complete response
/// TODO: better Debug response
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JsonRpcForwardedResponse {
    // TODO: jsonrpc a &str?
    pub jsonrpc: &'static str,
    pub id: Box<RawValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Arc<RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcErrorData>,
}

impl JsonRpcRequest {
    pub fn num_bytes(&self) -> usize {
        // TODO: not sure how to do this without wasting a ton of allocations
        serde_json::to_string(self)
            .expect("this should always be valid json")
            .len()
    }
}

impl JsonRpcForwardedResponse {
    pub fn from_anyhow_error(
        err: anyhow::Error,
        code: Option<i64>,
        id: Option<Box<RawValue>>,
    ) -> Self {
        let message = format!("{:?}", err);

        Self::from_string(message, code, id)
    }

    pub fn from_str(message: &str, code: Option<i64>, id: Option<Box<RawValue>>) -> Self {
        Self::from_string(message.to_string(), code, id)
    }

    pub fn from_string(message: String, code: Option<i64>, id: Option<Box<RawValue>>) -> Self {
        // TODO: this is too verbose. plenty of errors are valid, like users giving an invalid address. no need to log that
        // TODO: can we somehow get the initial request here? if we put that into a tracing span, will things slow down a ton?
        JsonRpcForwardedResponse {
            jsonrpc: "2.0",
            id: id.unwrap_or_default(),
            result: None,
            error: Some(JsonRpcErrorData {
                code: code.unwrap_or(-32099),
                message: message.into(),
                // TODO: accept data as an argument
                data: None,
            }),
        }
    }

    pub fn from_raw_response(result: Arc<RawValue>, id: Box<RawValue>) -> Self {
        JsonRpcForwardedResponse {
            jsonrpc: "2.0",
            id,
            // TODO: since we only use the result here, should that be all we return from try_send_request?
            result: Some(result),
            error: None,
        }
    }

    pub fn from_value(result: serde_json::Value, id: Box<RawValue>) -> Self {
        let partial_response = to_raw_value(&result).expect("Value to RawValue should always work");

        // TODO: an Arc is a waste here. change JsonRpcForwardedResponse to take an enum?
        let partial_response = partial_response.into();

        JsonRpcForwardedResponse {
            jsonrpc: "2.0",
            id,
            result: Some(partial_response),
            error: None,
        }
    }

    pub fn from_response_data(data: JsonRpcResponseEnum<Arc<RawValue>>, id: Box<RawValue>) -> Self {
        match data {
            JsonRpcResponseEnum::NullResult => {
                let x: Box<RawValue> = Default::default();
                Self::from_raw_response(x.into(), id)
            }
            JsonRpcResponseEnum::Result { value, .. } => Self::from_raw_response(value, id),
            JsonRpcResponseEnum::RpcError {
                error_data: value, ..
            } => JsonRpcForwardedResponse {
                jsonrpc: "2.0",
                id,
                result: None,
                error: Some(value),
            },
        }
    }
}

/// JSONRPC Responses can include one or many response objects.
#[derive(Clone, Debug, From, Serialize)]
#[serde(untagged)]
pub enum JsonRpcForwardedResponseEnum {
    Single(JsonRpcForwardedResponse),
    Batch(Vec<JsonRpcForwardedResponse>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_response() {
        let json = r#"{"jsonrpc":"2.0","id":null,"result":100}"#;
        let obj: ParsedResponse = serde_json::from_str(json).unwrap();
        assert!(matches!(obj.payload, Payload::Success { .. }));
    }

    #[test]
    fn serialize_response() {
        let obj = ParsedResponse {
            jsonrpc: "2.0".to_string(),
            id: None,
            payload: Payload::Success {
                result: serde_json::value::RawValue::from_string("100".to_string()).unwrap(),
            },
        };
        let json = serde_json::to_string(&obj).unwrap();
        assert_eq!(json, r#"{"jsonrpc":"2.0","id":null,"result":100}"#);
    }

    #[test]
    fn this_deserialize_single() {
        let input = r#"{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}"#;

        // test deserializing it directly to a single request object
        let output: JsonRpcRequest = serde_json::from_str(input).unwrap();

        assert_eq!(output.id.to_string(), "1");
        assert_eq!(output.method, "eth_blockNumber");
        assert_eq!(output.params.to_string(), "[]");

        // test deserializing it into an enum
        let output: JsonRpcRequestEnum = serde_json::from_str(input).unwrap();

        assert!(matches!(output, JsonRpcRequestEnum::Single(_)));
    }

    #[test]
    fn this_deserialize_batch() {
        let input = r#"[{"jsonrpc":"2.0","method":"eth_getCode","params":["0x5ba1e12693dc8f9c48aad8770482f4739beed696","0xe0e6a4"],"id":27},{"jsonrpc":"2.0","method":"eth_getTransactionCount","params":["0x5ba1e12693dc8f9c48aad8770482f4739beed696","0xe0e6a4"],"id":28},{"jsonrpc":"2.0","method":"eth_getBalance","params":["0x5ba1e12693dc8f9c48aad8770482f4739beed696","0xe0e6a4"],"id":29}]"#;

        // test deserializing it directly to a batch of request objects
        let output: Vec<JsonRpcRequest> = serde_json::from_str(input).unwrap();

        assert_eq!(output.len(), 3);

        assert_eq!(output[0].id.to_string(), "27");
        assert_eq!(output[0].method, "eth_getCode");
        assert_eq!(
            output[0].params.to_string(),
            r#"["0x5ba1e12693dc8f9c48aad8770482f4739beed696","0xe0e6a4"]"#
        );

        assert_eq!(output[1].id.to_string(), "28");
        assert_eq!(output[2].id.to_string(), "29");

        // test deserializing it into an enum
        let output: JsonRpcRequestEnum = serde_json::from_str(input).unwrap();

        assert!(matches!(output, JsonRpcRequestEnum::Batch(_)));
    }
}
