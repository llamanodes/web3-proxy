/// Helper for referral functions
/// Includes
///     - get referral link
///     - getting code for referral (shared and used)
use crate::TestApp;
use tracing::info;
use ulid::Ulid;
use web3_proxy::frontend::users::authentication::LoginPostResponse;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSharedReferralInfo {
    pub user: User,
    pub referrals: Vec<Referral>,
    pub used_referral_code: Ulid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserUsedReferralInfo {
    pub user: User,
    pub referrals: Vec<Referral>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub address: String,
    pub description: Option<String>,
    pub email: Option<String>,
    pub id: u64,
    pub user_tier_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Referral {
    pub credits_applied_for_referee: String,
    pub credits_applied_for_referrer: String,
    pub referral_start_date: String,
    pub referred_address: Option<String>,
    pub used_referral_code: Option<String>,
}

/// Helper function to create an "ordinary" user
#[allow(unused)]
pub async fn get_referral_code(
    x: &TestApp,
    r: &reqwest::Client,
    login_response: &LoginPostResponse,
) -> String {
    let get_referral_link = format!("{}user/referral", x.proxy_provider.url());

    // The referrer makes sure that the user is registered as a referred used
    let referral_link = r
        .get(&get_referral_link)
        .bearer_auth(login_response.bearer_token)
        .send()
        .await
        .unwrap();
    info!(?referral_link);
    let referral_link = referral_link.json::<serde_json::Value>().await.unwrap();
    info!("Referrer get link");
    info!(?referral_link);
    let referral_link = referral_link["referral_code"].as_str().unwrap().to_string();
    info!(?referral_link);

    referral_link
}

#[allow(unused)]
pub async fn get_shared_referral_codes(
    x: &TestApp,
    r: &reqwest::Client,
    login_response: &LoginPostResponse,
) -> UserSharedReferralInfo {
    let check_shared_referral_link =
        format!("{}user/referral/stats/shared-codes", x.proxy_provider.url());
    info!("Get balance");
    let shared_referral_codes = r
        .get(check_shared_referral_link)
        .bearer_auth(login_response.bearer_token)
        .send()
        .await
        .unwrap();
    info!(?shared_referral_codes);

    let shared_referral_codes = shared_referral_codes
        .json::<serde_json::Value>()
        .await
        .unwrap();
    info!(?shared_referral_codes);

    let user_referral_info: UserSharedReferralInfo =
        serde_json::from_value(shared_referral_codes).unwrap();

    user_referral_info
}

#[allow(unused)]
pub async fn get_used_referral_codes(
    x: &TestApp,
    r: &reqwest::Client,
    login_response: &LoginPostResponse,
) -> UserUsedReferralInfo {
    let check_used_referral_link =
        format!("{}user/referral/stats/used-codes", x.proxy_provider.url());
    info!("Get balance");
    let used_referral_codes = r
        .get(check_used_referral_link)
        .bearer_auth(login_response.bearer_token)
        .send()
        .await
        .unwrap();
    info!(?used_referral_codes);

    let used_referral_codes = used_referral_codes
        .json::<serde_json::Value>()
        .await
        .unwrap();
    info!(?used_referral_codes);

    let user_referral_info: UserUsedReferralInfo =
        serde_json::from_value(used_referral_codes).unwrap();
    user_referral_info
}
