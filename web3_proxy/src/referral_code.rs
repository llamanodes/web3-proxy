use anyhow::{self, Result};
use thread_fast_rng::rand;

pub struct ReferralCode(String);

impl Default for ReferralCode {
    fn default() -> Self {
        let rand_string = rand::thread_rng()
            .gen_ascii_chars()
            .take(32)
            .collect::<String>();
        Self(fmt!("llamanodes-{}", rand_string))
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
