use serde::{Deserialize, Serialize};
use std::borrow::Cow;

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

    pub fn is_retryable(&self) -> bool {
        // TODO: move stuff from request to here
        todo!()
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
