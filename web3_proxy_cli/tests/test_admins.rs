use std::str::FromStr;
use std::time::Duration;
use tracing::info;
use web3_proxy::prelude::migration::sea_orm::prelude::Decimal;
use web3_proxy::prelude::reqwest;
use web3_proxy::test_utils::mysql::TestMysql;
use web3_proxy::test_utils::TestAnvil;
use web3_proxy_cli::test_utils::admin_increases_balance::admin_increase_balance;
use web3_proxy_cli::test_utils::create_admin::create_user_as_admin;
use web3_proxy_cli::test_utils::create_user::create_user;
use web3_proxy_cli::test_utils::user_balance::user_get_balance;
use web3_proxy_cli::test_utils::TestApp;

// #[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[ignore = "under construction"]
#[test_log::test(tokio::test)]
async fn test_admin_imitate_user() {
    todo!();
}

#[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[test_log::test(tokio::test)]
async fn test_admin_grant_credits() {
    info!("Starting admin grant credits test");

    let a: TestAnvil = TestAnvil::spawn(31337).await;

    let db = TestMysql::spawn().await;

    let x = TestApp::spawn(&a, Some(&db), None, None).await;

    let r = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();

    // Setup variables that will be used
    let user_wallet = a.wallet(0);
    let admin_wallet = a.wallet(1);
    info!(?admin_wallet);

    let user_login_response = create_user(&x, &r, &user_wallet, None).await;
    let admin_login_response = create_user_as_admin(&x, &db, &r, &admin_wallet).await;
    info!(?admin_login_response);

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

    x.wait_for_stop();
}

// #[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[ignore = "under construction"]
#[test_log::test(tokio::test)]
async fn test_admin_change_user_tier() {
    todo!();
}
