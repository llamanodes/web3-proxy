use axum::headers::authorization::Bearer;
use ulid::Ulid;

/// Key used for caching the user's login
pub struct UserBearerToken(pub Ulid);

impl TryFrom<Bearer> for UserBearerToken {
    type Error = ulid::DecodeError;

    fn try_from(b: Bearer) -> Result<Self, ulid::DecodeError> {
        let u = Ulid::from_string(b.token())?;

        Ok(UserBearerToken(u))
    }
}

impl ToString for UserBearerToken {
    fn to_string(&self) -> String {
        format!("bearer:{}", self.0)
    }
}
