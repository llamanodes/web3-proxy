use axum::response::Response as AxumResponse;
use derive_more::From;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_inline_default::serde_inline_default;
use serde_json::value::RawValue;
use std::fmt;
use std::sync::{atomic, Arc};
use std::time::Duration;
use tokio::time::sleep;

use crate::app::Web3ProxyApp;
use crate::errors::Web3ProxyError;
use crate::frontend::authorization::{Authorization, RequestOrMethod, Web3Request};

use super::LooseId;

// TODO: &str here instead of String should save a lot of allocations
// TODO: generic type for params?
#[serde_inline_default]
#[derive(Clone, Deserialize, Serialize)]
pub struct SingleRequest {
    pub jsonrpc: String,
    /// id could be a stricter type, but many rpcs do things against the spec
    /// TODO: this gets cloned into the response object often. would an Arc be better? That has its own overhead and these are short strings
    pub id: Box<RawValue>,
    pub method: String,
    #[serde_inline_default(serde_json::Value::Null)]
    pub params: serde_json::Value,
}

impl SingleRequest {
    // TODO: Web3ProxyResult? can this even fail?
    pub fn new(id: LooseId, method: String, params: serde_json::Value) -> anyhow::Result<Self> {
        let x = Self {
            jsonrpc: "2.0".to_string(),
            id: id.to_raw_value(),
            method,
            params,
        };

        Ok(x)
    }

    /// TODO: not sure how to do this without wasting a ton of allocations
    pub fn num_bytes(&self) -> usize {
        serde_json::to_string(self)
            .expect("this should always be valid json")
            .len()
    }

    pub fn validate_method(&self) -> bool {
        self.method
            .chars()
            .all(|x| x.is_ascii_alphanumeric() || x == '_' || x == '(' || x == ')')
    }
}

impl fmt::Debug for SingleRequest {
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
    Batch(Vec<SingleRequest>),
    Single(SingleRequest),
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
        app: &Arc<Web3ProxyApp>,
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

        // TODO: create a stat so we can penalize
        // TODO: what request size
        let request = Web3Request::new_with_app(
            app,
            authorization.clone(),
            None,
            RequestOrMethod::Method("invalid_method".into(), size),
            None,
        )
        .await
        .unwrap();

        request
            .user_error_response
            .store(true, atomic::Ordering::Release);

        let response = Web3ProxyError::BadRequest("request failed validation".into());

        request.add_response(&response);

        let response = response.into_response_with_id(Some(err_id));

        // TODO: variable duration depending on the IP
        sleep(duration).await;

        let _ = request.try_send_arc_stat();

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
                let mut batch: Vec<SingleRequest> =
                    Vec::with_capacity(seq.size_hint().unwrap_or(10));

                while let Ok(Some(s)) = seq.next_element::<SingleRequest>() {
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

                let single = SingleRequest {
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
