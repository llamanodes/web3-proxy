mod common;

use ethers::prelude::{Http, JsonRpcClientWrapper, Provider};
use std::time::Duration;

use crate::common::create_provider_with_rpc_key::create_provider_for_user;
use crate::common::rpc_key::user_get_first_rpc_key;
use crate::common::user_balance::user_get_balance;
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

    // Since when do indices start with 1
    let x_0 = TestApp::spawn(&a, Some(&db)).await;
    let x_1 = TestApp::spawn(&a, Some(&db)).await;

    // test the basics
    let anvil_provider = &a.provider;

    let anvil_result = anvil_provider
        .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
        .await
        .unwrap()
        .unwrap();

    // make a user and give them credits
    let user_0_wallet = a.wallet(0);
    let user_1_wallet = a.wallet(1);
    let admin_wallet = a.wallet(2);
    info!(?admin_wallet);

    // Login both users
    let user_0_login = create_user(&x_0, &r, &user_0_wallet, None).await;
    let user_1_login = create_user(&x_1, &r, &user_1_wallet, None).await;
    let admin_login = create_user_as_admin(&x_0, &db, &r, &admin_wallet).await;

    // Load up balances
    admin_increase_balance(&x_0, &r, &admin_login, &user_0_wallet, Decimal::from(20)).await;
    admin_increase_balance(&x_1, &r, &admin_login, &user_1_wallet, Decimal::from(30)).await;
    // Just check that balances were properly distributed
    let user_0_balance_response = user_get_balance(&x_0, &r, &user_0_login).await;
    let user_0_balance_pre = user_0_balance_response.remaining();
    let user_1_balance_response = user_get_balance(&x_1, &r, &user_1_login).await;
    let user_1_balance_pre = user_1_balance_response.remaining();

    // Make sure they both have balance now
    assert_eq!(user_0_balance_pre, Decimal::from(20));
    assert_eq!(user_1_balance_pre, Decimal::from(20));

    // Generate the proxies

    // TODO: Bryan? IDK if necessary, prob should go on top
    // assert_eq!(anvil_result, proxy_1_result);
    // assert_eq!(anvil_result, proxy_2_result);

    let number_requests = 50;
    let mut handles = Vec::new();

    // Get the RPC key from the user
    let user_0_secret_key = user_get_first_rpc_key(&x_0, &r, &user_0_login)
        .await
        .secret_key;
    let user_1_secret_key = user_get_first_rpc_key(&x_1, &r, &user_1_login)
        .await
        .secret_key;

    for _ in 1..number_requests {
        let proxy_0_url = x_0.proxy_provider.url().clone();
        handles.push(tokio::spawn(async move {
            // Because of the move, we re-create the provider each time; this should be fine because it's http
            let proxy_provider_0 =
                create_provider_for_user(proxy_0_url.clone(), user_0_secret_key.clone()).await;
            proxy_provider_0
                .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
                .await
                .unwrap()
                .unwrap()
        }));
        let proxy_1_url = x_1.proxy_provider.url().clone();
        handles.push(tokio::spawn(async move {
            let proxy_provider_1 =
                create_provider_for_user(proxy_1_url.clone(), user_1_secret_key.clone()).await;
            proxy_provider_1
                .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
                .await
                .unwrap()
                .unwrap()
        }));
    }

    // Flush all stats here
    let flush_count = x_0.flush_stats().await.unwrap();
    assert_eq!(flush_count.timeseries, 0);
    assert!(flush_count.relational > 0);
    let flush_count = x_1.flush_stats().await.unwrap();
    assert_eq!(flush_count.timeseries, 0);
    assert!(flush_count.relational > 0);

    // TODO: Need to validate all the stat accounting now
}
