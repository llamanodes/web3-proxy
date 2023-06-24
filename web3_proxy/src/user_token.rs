use axum::headers::authorization::Bearer;
use migration::sea_orm::prelude::Uuid;
use serde::Serialize;
use std::fmt;
use std::str::FromStr;
use ulid::Ulid;

/// Key used for caching the user's login
#[derive(Clone, Hash, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct UserBearerToken(pub Ulid);

impl UserBearerToken {
    pub fn redis_key(&self) -> String {
        format!("bearer:{}", self.0)
    }

    pub fn uuid(&self) -> Uuid {
        Uuid::from_u128(self.0.into())
    }
}

impl Default for UserBearerToken {
    fn default() -> Self {
        Self(Ulid::new())
    }
}

impl FromStr for UserBearerToken {
    type Err = ulid::DecodeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let ulid = Ulid::from_str(s)?;

        Ok(Self(ulid))
    }
}

impl From<Ulid> for UserBearerToken {
    fn from(x: Ulid) -> Self {
        Self(x)
    }
}

impl From<UserBearerToken> for Uuid {
    fn from(x: UserBearerToken) -> Self {
        x.uuid()
    }
}

impl fmt::Display for UserBearerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl TryFrom<Bearer> for UserBearerToken {
    type Error = ulid::DecodeError;

    fn try_from(b: Bearer) -> Result<Self, ulid::DecodeError> {
        let u = Ulid::from_string(b.token())?;

        Ok(UserBearerToken(u))
    }
}
