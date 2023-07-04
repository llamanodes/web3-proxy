mod common;

use std::str::FromStr;
use std::time::Duration;

use crate::common::admin_increases_balance::admin_increase_balance;
use crate::common::create_admin::create_user_as_admin;
use crate::common::create_user::create_user;
use crate::common::get_user_balance::user_get_balance;
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

    let (user_wallet, user_login_response) = create_user(&x, &r).await;
    let (admin_wallet, admin_login_response) = create_user_as_admin(&x, &r).await;
    info!(?admin_wallet);
    info!(?admin_login_response);

    let (increase_balance_response) = admin_increase_balance(
        &x,
        &r,
        &admin_login_response,
        &user_wallet,
        Decimal::from(100),
    )
    .await;
    // First assert
    assert_eq!(
        Decimal::from_str(increase_balance_response["amount"].as_str().unwrap()).unwrap(),
        Decimal::from(100)
    );

    // TODO: We should actually check the user's balance here ...
    let user_balance_response = user_get_balance(&x, &r, &user_login_response).await;
    assert_eq!(
        Decimal::from_str(user_balance_response["balance"].as_str().unwrap()).unwrap(),
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
