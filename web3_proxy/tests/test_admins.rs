mod common;

use std::str::FromStr;
use std::time::Duration;

use crate::common::create_admin::create_user_as_admin;
use crate::common::create_user::create_user;
use crate::common::TestApp;
use ethers::prelude::Signer;
use ethers::types::Signature;
use rust_decimal::Decimal;
use tracing::info;
use web3_proxy::frontend::admin::AdminIncreaseBalancePost;
use web3_proxy::frontend::users::authentication::{LoginPostResponse, PostLogin};
use web3_proxy::sub_commands::ChangeAdminStatusSubCommand;

// #[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[ignore = "under construction"]
#[test_log::test(tokio::test)]
async fn test_admin_imitate_user() {
    let x = TestApp::spawn(true).await;

    todo!();
}

#[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[test_log::test(tokio::test)]
async fn test_admin_grant_credits() {
    info!("Starting admin grant credits test");
    let x = TestApp::spawn(true).await;
    let r = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();

    // Setup variables that will be used
    let increase_balance_post_url = format!("{}admin/increase_balance", x.proxy_provider.url());

    let (user_wallet, _) = create_user(&x, &r).await;
    let (admin_wallet, admin_login_response) = create_user_as_admin(&x, &r).await;
    info!(?admin_wallet);
    info!(?admin_login_response);

    info!("Increasing balance");
    // Login the user
    // Use the bearer token of admin to increase user balance
    let increase_balance_data = AdminIncreaseBalancePost {
        user_address: user_wallet.address(), // set user address to increase balance
        amount: Decimal::from(100),          // set amount to increase
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

    // Check if the response is as expected
    // TODO: assert_eq!(increase_balance_response["user"], user_wallet.address());
    assert_eq!(
        Decimal::from_str(increase_balance_response["amount"].as_str().unwrap()).unwrap(),
        Decimal::from(100)
    );

    x.wait().await;
}

// #[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[ignore = "under construction"]
#[test_log::test(tokio::test)]
async fn test_admin_change_user_tier() {
    let x = TestApp::spawn(true).await;
    todo!();
}
