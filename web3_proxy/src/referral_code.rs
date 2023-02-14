use anyhow::{self, Result};
use thread_fast_rng::rand::distributions::Alphanumeric;
use thread_fast_rng::rand::{Rng, thread_rng};

pub struct ReferralCode(pub String);

impl Default for ReferralCode {
    fn default() -> Self {
        let mut rng = thread_rng();
        let chars: String = (0..32).map(|_| rng.sample(Alphanumeric) as char).collect();
        Self(format!("llamanodes-{}", chars))
    }
}

impl TryFrom<String> for ReferralCode {
    type Error = anyhow::Error;

    fn try_from(x: String) -> Result<Self> {
        if !x.starts_with("llamanodes-") {
            return Err(anyhow::anyhow!("Referral Code does not have the right format"));
        }
        Ok(Self(x))
    }

}
