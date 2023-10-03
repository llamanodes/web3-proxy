use serde::{Deserialize, Serialize};
use std::fmt;
use ulid::Ulid;
use uuid::Uuid;

/// This lets us use UUID and ULID while we transition to only ULIDs
/// TODO: custom deserialize that can also go from String to Ulid
#[derive(Copy, Clone, Deserialize)]
pub enum RpcSecretKey {
    Ulid(Ulid),
    Uuid(Uuid),
}

impl RpcSecretKey {
    pub fn new() -> Self {
        Ulid::new().into()
    }

    pub fn as_128(&self) -> u128 {
        match self {
            Self::Ulid(x) => x.0,
            Self::Uuid(x) => x.as_u128(),
        }
    }
}

impl PartialEq for RpcSecretKey {
    fn eq(&self, other: &Self) -> bool {
        self.as_128() == other.as_128()
    }
}

impl Eq for RpcSecretKey {}

impl fmt::Debug for RpcSecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ulid(x) => fmt::Debug::fmt(x, f),
            Self::Uuid(x) => {
                let x = Ulid::from(x.as_u128());

                fmt::Debug::fmt(&x, f)
            }
        }
    }
}

/// always serialize as a ULID.
impl Serialize for RpcSecretKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Ulid(x) => x.serialize(serializer),
            Self::Uuid(x) => {
                let x: Ulid = x.to_owned().into();

                x.serialize(serializer)
            }
        }
    }
}
