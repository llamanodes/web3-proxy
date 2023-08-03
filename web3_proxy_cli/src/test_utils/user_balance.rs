use super::TestApp;
use tracing::{info, trace};
use web3_proxy::balance::Balance;
use web3_proxy::frontend::users::authentication::LoginPostResponse;
use web3_proxy::prelude::reqwest;
use web3_proxy::prelude::serde_json::json;

/// Helper function to get the user's balance
#[allow(unused)]
pub async fn user_get_balance(
    x: &TestApp,
    r: &reqwest::Client,
    login_response: &LoginPostResponse,
) -> Balance {
    let get_user_balance = format!("{}user/balance", x.proxy_provider.url());

    let balance_response = r
        .get(get_user_balance)
        .bearer_auth(login_response.bearer_token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    trace!(
        ?balance_response,
        "get balance for user #{}",
        login_response.user.id
    );

    let balance = balance_response.json().await.unwrap();

    info!("balance: {:#}", json!(&balance));

    balance
}
