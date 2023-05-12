use anyhow::{self, Result};
use ulid::Ulid;

pub struct ReferralCode(pub String);

impl Default for ReferralCode {
    fn default() -> Self {
        let out = Ulid::new();
        Self(format!("llamanodes-{}", out))
    }
}

impl TryFrom<String> for ReferralCode {
    type Error = anyhow::Error;

    fn try_from(x: String) -> Result<Self> {
        if !x.starts_with("llamanodes-") {
            return Err(anyhow::anyhow!(
                "Referral Code does not have the right format"
            ));
        }
        Ok(Self(x))
    }
}
