mod common;

use std::str::FromStr;
use std::time::Duration;

use crate::common::admin_increases_balance::admin_increase_balance;
use crate::common::create_admin::create_user_as_admin;
use crate::common::create_user::{create_user, set_user_tier};
use crate::common::user_balance::user_get_balance;
use crate::common::TestApp;
use migration::sea_orm::prelude::Decimal;
use tracing::info;

// #[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[ignore = "under construction"]
#[test_log::test(tokio::test)]
async fn test_admin_imitate_user() {
    let x = TestApp::spawn(31337, true).await;

    todo!();
}

#[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[test_log::test(tokio::test)]
async fn test_admin_grant_credits() {
    info!("Starting admin grant credits test");
    let x = TestApp::spawn(31337, true).await;
    let r = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();

    // Setup variables that will be used
    let user_wallet = x.wallet(0);
    let admin_wallet = x.wallet(1);
    info!(?admin_wallet);

    let user_login_response = create_user(&x, &r, &user_wallet, None).await;
    let admin_login_response = create_user_as_admin(&x, &r, &admin_wallet).await;
    info!(?admin_login_response);

    set_user_tier(&x, user_login_response.user.clone(), "Premium").await.unwrap();

    let increase_balance_response = admin_increase_balance(
        &x,
        &r,
        &admin_login_response,
        &user_wallet,
        Decimal::from(100),
    )
    .await;

    assert_eq!(
        Decimal::from_str(increase_balance_response["amount"].as_str().unwrap()).unwrap(),
        Decimal::from(100)
    );

    let user_balance = user_get_balance(&x, &r, &user_login_response).await;
    assert_eq!(user_balance.remaining(), Decimal::from(100));

    x.wait().await;
}

// #[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[ignore = "under construction"]
#[test_log::test(tokio::test)]
async fn test_admin_change_user_tier() {
    let x = TestApp::spawn(31337, true).await;
    todo!();
}
