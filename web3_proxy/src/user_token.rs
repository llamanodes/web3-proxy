use std::str::FromStr;

use axum::headers::authorization::Bearer;
use migration::sea_orm::prelude::Uuid;
use serde::Serialize;
use ulid::Ulid;
use tracing::{instrument};

/// Key used for caching the user's login
#[derive(Clone, Debug, Hash, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct UserBearerToken(pub Ulid);

impl UserBearerToken {
    #[instrument(level = "trace")]
    pub fn redis_key(&self) -> String {
        format!("bearer:{}", self.0)
    }

    #[instrument(level = "trace")]
    pub fn uuid(&self) -> Uuid {
        Uuid::from_u128(self.0.into())
    }
}

impl Default for UserBearerToken {
    #[instrument(level = "trace")]
    fn default() -> Self {
        Self(Ulid::new())
    }
}

impl FromStr for UserBearerToken {
    type Err = ulid::DecodeError;

    #[instrument(level = "trace")]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let ulid = Ulid::from_str(s)?;

        Ok(Self(ulid))
    }
}

impl From<Ulid> for UserBearerToken {
    #[instrument(level = "trace")]
    fn from(x: Ulid) -> Self {
        Self(x)
    }
}

impl From<UserBearerToken> for Uuid {
    #[instrument(level = "trace")]
    fn from(x: UserBearerToken) -> Self {
        x.uuid()
    }
}

impl TryFrom<Bearer> for UserBearerToken {
    type Error = ulid::DecodeError;

    #[instrument(level = "trace")]
    fn try_from(b: Bearer) -> Result<Self, ulid::DecodeError> {
        let u = Ulid::from_string(b.token())?;

        Ok(UserBearerToken(u))
    }
}
