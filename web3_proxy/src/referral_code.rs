use std::fmt::Display;

use anyhow::{self, Result};
use ulid::Ulid;

pub struct ReferralCode(String);

impl Default for ReferralCode {
    fn default() -> Self {
        let out = Ulid::new();
        Self(format!("llamanodes-{}", out))
    }
}

impl Display for ReferralCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
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
