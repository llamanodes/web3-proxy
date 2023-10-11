use derive_more::From;
use serde_json::{json, value::RawValue};

/// being strict on id doesn't really help much. just accept anything
#[derive(From)]
pub enum LooseId {
    None,
    Number(u64),
    String(String),
    Raw(Box<RawValue>),
}

impl LooseId {
    pub fn to_raw_value(self) -> Box<RawValue> {
        // TODO: is this a good way to do this? maybe also have `as_raw_value`?
        match self {
            Self::None => Default::default(),
            Self::Number(x) => {
                serde_json::from_value(json!(x)).expect("number id should always work")
            }
            Self::String(x) => serde_json::from_str(&x).expect("string id should always work"),
            Self::Raw(x) => x,
        }
    }
}
