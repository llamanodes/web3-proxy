use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::Serialize;
use serde_json::value::RawValue;
use std::fmt;

#[derive(Clone, serde::Deserialize)]
pub struct JsonRpcRequest {
    // TODO: skip jsonrpc entireley?
    // pub jsonrpc: Box<RawValue>,
    pub id: Box<RawValue>,
    pub method: String,
    pub params: Box<RawValue>,
}

impl fmt::Debug for JsonRpcRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("JsonRpcRequest")
            .field("id", &self.id)
            .field("method", &self.method)
            .field("params", &self.params)
            .finish()
    }
}

/// Requests can come in multiple formats
#[derive(Debug)]
pub enum JsonRpcRequestEnum {
    Batch(Vec<JsonRpcRequest>),
    Single(JsonRpcRequest),
}

impl<'de> Deserialize<'de> for JsonRpcRequestEnum {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        #[serde(field_identifier, rename_all = "lowercase")]
        enum Field {
            JsonRpc,
            Id,
            Method,
            Params,
            // TODO: jsonrpc here, too?
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
                let mut id = None;
                let mut method = None;
                let mut params = None;

                while let Some(key) = map.next_key()? {
                    match key {
                        Field::JsonRpc => {
                            // throw away the value
                            // TODO: should we check that it's 2.0?
                            // TODO: how do we skip over this value entirely?
                            let _: String = map.next_value()?;
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

                let id = id.ok_or_else(|| de::Error::missing_field("id"))?;
                let method = method.ok_or_else(|| de::Error::missing_field("method"))?;
                let params = params.ok_or_else(|| de::Error::missing_field("params"))?;

                let single = JsonRpcRequest { id, method, params };

                Ok(JsonRpcRequestEnum::Single(single))
            }
        }

        let batch_visitor = JsonRpcBatchVisitor {};

        deserializer.deserialize_any(batch_visitor)
    }
}

/// All jsonrpc errors use this structure
#[derive(Serialize, Clone)]
pub struct JsonRpcErrorData {
    /// The error code
    pub code: i64,
    /// The error message
    pub message: String,
    /// Additional data
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// A complete response
#[derive(Clone, Serialize)]
pub struct JsonRpcForwardedResponse {
    pub jsonrpc: String,
    pub id: Box<RawValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Box<RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcErrorData>,
}

/// TODO: the default formatter takes forever to write. this is too quiet though
impl fmt::Debug for JsonRpcForwardedResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JsonRpcForwardedResponse")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// JSONRPC Responses can include one or many response objects.
#[derive(Clone, Serialize)]
#[serde(untagged)]
pub enum JsonRpcForwardedResponseEnum {
    Single(JsonRpcForwardedResponse),
    Batch(Vec<JsonRpcForwardedResponse>),
}

#[cfg(test)]
mod tests {
    use super::*;

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

        assert_eq!(0, 1);
    }
}
