use crate::TestApp;
use tracing::trace;
use web3_proxy::frontend::users::authentication::LoginPostResponse;

/// Helper function to increase the balance of a user, from an admin
#[allow(unused)]
pub async fn get_admin_deposits(
    x: &TestApp,
    r: &reqwest::Client,
    user: &LoginPostResponse,
) -> serde_json::Value {
    let increase_balance_post_url = format!("{}user/deposits/admin", x.proxy_provider.url());
    trace!("Get admin increase deposits");
    // Login the user
    // Use the bearer token of admin to increase user balance
    let admin_balance_deposits = r
        .get(increase_balance_post_url)
        .bearer_auth(user.bearer_token)
        .send()
        .await
        .unwrap();
    trace!(?admin_balance_deposits, "http response");
    let admin_balance_deposits = admin_balance_deposits
        .json::<serde_json::Value>()
        .await
        .unwrap();
    trace!(?admin_balance_deposits, "json response");

    admin_balance_deposits
}
