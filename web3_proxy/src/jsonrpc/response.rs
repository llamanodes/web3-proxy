use super::JsonRpcErrorData;
use crate::errors::{Web3ProxyError, Web3ProxyResult};
use crate::jsonrpc::ValidatedRequest;
use crate::response_cache::ForwardedResponse;
use axum::body::StreamBody;
use axum::response::IntoResponse;
use axum::Json;
use bytes::{Bytes, BytesMut};
use futures_util::stream::{self, StreamExt};
use futures_util::TryStreamExt;
use serde::{de, Deserialize, Serialize};
use serde_json::value::RawValue;
use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

pub trait JsonRpcParams = fmt::Debug + serde::Serialize + Send + Sync + 'static;
pub trait JsonRpcResultData = serde::Serialize + serde::de::DeserializeOwned + fmt::Debug + Send;

/// TODO: borrow values to avoid allocs if possible
/// TODO: lots of overlap with `SingleForwardedResponse`
#[derive(Debug, Serialize)]
pub struct ParsedResponse<T = Arc<RawValue>> {
    pub jsonrpc: String,
    pub id: Box<RawValue>,
    #[serde(flatten)]
    pub payload: ResponsePayload<T>,
}

impl ParsedResponse {
    pub fn from_value(value: serde_json::Value, id: Box<RawValue>) -> Self {
        let result = serde_json::value::to_raw_value(&value)
            .expect("this should not fail")
            .into();
        Self::from_result(result, id)
    }
}

impl ParsedResponse<Arc<RawValue>> {
    pub fn from_response_data(data: ForwardedResponse<Arc<RawValue>>, id: Box<RawValue>) -> Self {
        match data {
            ForwardedResponse::NullResult => {
                let x: Box<RawValue> = Default::default();
                // TODO: how can we make this generic if this always wants to be a Box<RawValue>?. Do we even want to keep NullResult?
                Self::from_result(Arc::from(x), id)
            }
            ForwardedResponse::RpcError { error_data, .. } => Self::from_error(error_data, id),
            ForwardedResponse::Result { value, .. } => Self::from_result(value, id),
        }
    }
}

impl<T> ParsedResponse<T> {
    pub fn from_result(result: T, id: Box<RawValue>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            payload: ResponsePayload::Success { result },
        }
    }

    pub fn from_error(error: JsonRpcErrorData, id: Box<RawValue>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            payload: ResponsePayload::Error { error },
        }
    }

    pub fn result(&self) -> Option<&T> {
        match &self.payload {
            ResponsePayload::Success { result } => Some(result),
            ResponsePayload::Error { .. } => None,
        }
    }

    pub fn into_result(self) -> Web3ProxyResult<T> {
        match self.payload {
            ResponsePayload::Success { result } => Ok(result),
            ResponsePayload::Error { error } => Err(Web3ProxyError::JsonRpcErrorData(error)),
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

                let id = id.unwrap_or_default();

                // jsonrpc version must be present in all responses
                let jsonrpc = jsonrpc
                    .ok_or_else(|| de::Error::missing_field("jsonrpc"))?
                    .to_string();

                let payload = match (result, error) {
                    (Some(result), None) => ResponsePayload::Success { result },
                    (None, Some(error)) => ResponsePayload::Error { error },
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
pub enum ResponsePayload<T> {
    Success { result: T },
    Error { error: JsonRpcErrorData },
}

#[derive(Debug)]
pub struct StreamResponse<T> {
    _t: PhantomData<T>,
    buffer: Bytes,
    response: reqwest::Response,
    web3_request: Arc<ValidatedRequest>,
}

impl<T> StreamResponse<T> {
    // TODO: error handing
    pub async fn read(self) -> Web3ProxyResult<ParsedResponse<T>>
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

impl<T> IntoResponse for StreamResponse<T> {
    fn into_response(self) -> axum::response::Response {
        let stream = stream::once(async { Ok::<_, reqwest::Error>(self.buffer) })
            .chain(self.response.bytes_stream())
            .map_ok(move |x| {
                let len = x.len();

                self.web3_request.add_response(len);

                x
            });
        let body = StreamBody::new(stream);
        body.into_response()
    }
}

#[derive(Debug)]
pub enum SingleResponse<T = Arc<RawValue>> {
    /// TODO: save the size here so we don't have to serialize again
    Parsed(ParsedResponse<T>),
    Stream(StreamResponse<T>),
}

impl<T> SingleResponse<T>
where
    T: de::DeserializeOwned + Serialize,
{
    // TODO: threshold from configs
    // TODO: error handling
    // TODO: if a large stream's response's initial chunk "error" then we should buffer it
    pub async fn read_if_short(
        mut response: reqwest::Response,
        nbytes: u64,
        web3_request: &Arc<ValidatedRequest>,
    ) -> Web3ProxyResult<SingleResponse<T>> {
        match response.content_length() {
            // short
            Some(len) if len <= nbytes => Ok(Self::from_bytes(response.bytes().await?)?),
            // long
            Some(_) => Ok(Self::Stream(StreamResponse {
                buffer: Bytes::new(),
                response,
                web3_request: web3_request.clone(),
            })),
            // unknown length. maybe compressed. maybe streaming. maybe both
            None => {
                let mut buffer = BytesMut::new();
                while (buffer.len() as u64) < nbytes {
                    match response.chunk().await? {
                        Some(chunk) => {
                            buffer.extend_from_slice(&chunk);
                        }
                        None => {
                            // it was short
                            return Ok(Self::from_bytes(buffer.freeze())?);
                        }
                    }
                }

                // we've read nbytes of the response, but there is more to come
                let buffer = buffer.freeze();
                Ok(Self::Stream(StreamResponse {
                    buffer,
                    response,
                    web3_request: web3_request.clone(),
                }))
            }
        }
    }

    fn from_bytes(buf: Bytes) -> Result<Self, serde_json::Error> {
        let val = serde_json::from_slice(&buf)?;
        Ok(Self::Parsed(val))
    }

    // TODO: error handling
    pub async fn parsed(self) -> Web3ProxyResult<ParsedResponse<T>> {
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

    pub fn set_id(&mut self, id: Box<RawValue>) {
        match self {
            SingleResponse::Parsed(x) => {
                x.id = id;
            }
            SingleResponse::Stream(..) => {
                // stream responses will hopefully always have the right id already because we pass the orignal id all the way from the front to the back
            }
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

impl Response<Arc<RawValue>> {
    pub async fn to_json_string(self) -> Web3ProxyResult<String> {
        let x = match self {
            Self::Single(resp) => {
                // TODO: handle streaming differently?
                let parsed = resp.parsed().await?;

                serde_json::to_string(&parsed)
            }
            Self::Batch(resps) => serde_json::to_string(&resps),
        };

        let x = x.expect("to_string should always work");

        Ok(x)
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
