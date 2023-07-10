use crate::TestApp;
use ethers::prelude::{LocalWallet, Signer};
use migration::sea_orm::prelude::Decimal;
use tracing::info;
use web3_proxy::frontend::admin::AdminIncreaseBalancePost;
use web3_proxy::frontend::users::authentication::LoginPostResponse;

/// Helper function to increase the balance of a user, from an admin
#[allow(unused)]
pub async fn admin_increase_balance(
    x: &TestApp,
    r: &reqwest::Client,
    admin_login_response: &LoginPostResponse,
    target_wallet: &LocalWallet,
    amount: Decimal,
) -> serde_json::Value {
    let increase_balance_post_url = format!("{}admin/increase_balance", x.proxy_provider.url());
    info!("Increasing balance");
    // Login the user
    // Use the bearer token of admin to increase user balance
    let increase_balance_data = AdminIncreaseBalancePost {
        user_address: target_wallet.address(), // set user address to increase balance
        amount,                                // set amount to increase
        note: Some("Test increasing balance".to_string()),
    };
    info!(?increase_balance_post_url);
    info!(?increase_balance_data);
    info!(?admin_login_response.bearer_token);

    let increase_balance_response = r
        .post(increase_balance_post_url)
        .json(&increase_balance_data)
        .bearer_auth(admin_login_response.bearer_token)
        .send()
        .await
        .unwrap();
    info!(?increase_balance_response, "http response");

    let increase_balance_response = increase_balance_response
        .json::<serde_json::Value>()
        .await
        .unwrap();
    info!(?increase_balance_response, "json response");

    increase_balance_response
}
