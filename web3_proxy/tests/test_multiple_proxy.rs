mod common;

use std::time::Duration;

use crate::common::{
    admin_increases_balance::admin_increase_balance, anvil::TestAnvil,
    create_admin::create_user_as_admin, create_user::create_user, mysql::TestMysql, TestApp,
};
use rust_decimal::Decimal;
use tracing::info;
use web3_proxy::rpcs::blockchain::ArcBlock;

// #[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[test_log::test(tokio::test)]
async fn test_multiple_proxies_stats_add_up() {
    let a = TestAnvil::spawn(999_001_999).await;

    let db = TestMysql::spawn().await;

    let r = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap();

    let x_1 = TestApp::spawn(&a, Some(&db)).await;
    let x_2 = TestApp::spawn(&a, Some(&db)).await;

    // test the basics
    let anvil_provider = &a.provider;
    let proxy_provider_1 = &x_1.proxy_provider;
    let proxy_provider_2 = &x_2.proxy_provider;

    let anvil_result = anvil_provider
        .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
        .await
        .unwrap()
        .unwrap();
    let proxy_1_result = proxy_provider_1
        .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
        .await
        .unwrap()
        .unwrap();
    let proxy_2_result = proxy_provider_2
        .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(anvil_result, proxy_1_result);
    assert_eq!(anvil_result, proxy_2_result);

    // make a user and give them credits
    let user_wallet = a.wallet(0);
    let admin_wallet = a.wallet(1);
    info!(?admin_wallet);

    // login at proxy 1
    let user_login_response = create_user(&x_1, &r, &user_wallet, None).await;
    let admin_login_response = create_user_as_admin(&x_1, &db, &r, &admin_wallet).await;
    info!(?admin_login_response);

    // increase balance at proxy 2
    let increase_balance_response = admin_increase_balance(
        &x_2,
        &r,
        &admin_login_response,
        &user_wallet,
        Decimal::from(1000),
    )
    .await;

    todo!("make some rpc requests on both servers");
}
