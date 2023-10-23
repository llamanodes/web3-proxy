pub mod error;
pub mod id;
pub mod request;
pub mod request_builder;
pub mod response;

use std::fmt;

pub use self::error::JsonRpcErrorData;
pub use self::id::LooseId;
pub use self::request::{JsonRpcRequestEnum, SingleRequest};
pub use self::response::{
    ParsedResponse, Response, ResponsePayload, SingleResponse, StreamResponse,
};
pub use request_builder::ValidatedRequest;

pub trait JsonRpcParams = fmt::Debug + serde::Serialize + Send + Sync + 'static;
pub trait JsonRpcResultData = serde::Serialize + serde::de::DeserializeOwned + fmt::Debug + Send;

#[cfg(test)]
mod tests {
    use super::request::{JsonRpcRequestEnum, SingleRequest};
    use super::response::{ParsedResponse, ResponsePayload};

    #[test]
    fn deserialize_response() {
        let json = r#"{"jsonrpc":"2.0","id":null,"result":100}"#;
        let obj: ParsedResponse = serde_json::from_str(json).unwrap();
        assert!(matches!(obj.payload, ResponsePayload::Success { .. }));
    }

    #[test]
    fn serialize_response() {
        let obj = ParsedResponse {
            jsonrpc: "2.0".into(),
            id: Default::default(),
            payload: ResponsePayload::Success {
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
        let output: SingleRequest = serde_json::from_str(input).unwrap();

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
        let output: Vec<SingleRequest> = serde_json::from_str(input).unwrap();

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
