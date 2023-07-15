mod common;

use crate::common::create_provider_with_rpc_key::create_provider_for_user;
use crate::common::rpc_key::user_get_first_rpc_key;
use crate::common::user_balance::user_get_balance;
use crate::common::{
    admin_increases_balance::admin_increase_balance, anvil::TestAnvil,
    create_admin::create_user_as_admin, create_user::create_user, mysql::TestMysql, TestApp,
};
use futures::future::{join_all, try_join_all};
use rust_decimal::Decimal;
use std::sync::Arc;
use std::time::Duration;
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
    admin_increase_balance(&x_0, &r, &admin_login, &user_0_wallet, Decimal::from(1000)).await;
    admin_increase_balance(&x_1, &r, &admin_login, &user_1_wallet, Decimal::from(2000)).await;

    let user_0_balance = user_get_balance(&x_0, &r, &user_0_login).await;
    let user_1_balance = user_get_balance(&x_1, &r, &user_1_login).await;

    let user_0_balance_pre = user_0_balance.remaining();
    let user_1_balance_pre = user_1_balance.remaining();

    assert_eq!(user_0_balance_pre, Decimal::from(1000));
    assert_eq!(user_1_balance_pre, Decimal::from(2000));

    // Generate the proxies
    let number_requests = 50;
    let mut handles = Vec::new();

    // Get the RPC key from the user
    let user_0_secret_key = user_get_first_rpc_key(&x_0, &r, &user_0_login)
        .await
        .secret_key;

    let proxy_0_user_0_provider =
        create_provider_for_user(x_0.proxy_provider.url(), &user_0_secret_key).await;
    let proxy_1_user_0_provider =
        create_provider_for_user(x_1.proxy_provider.url(), &user_0_secret_key).await;

    let proxy_0_user_0_provider = Arc::new(proxy_0_user_0_provider);
    let proxy_1_user_0_provider = Arc::new(proxy_1_user_0_provider);

    for _ in 0..number_requests {
        // send 2 to proxy 0 user 0
        let proxy_0_user_0_provider_clone = proxy_0_user_0_provider.clone();
        handles.push(tokio::spawn(async move {
            proxy_0_user_0_provider_clone
                .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
                .await
                .unwrap()
                .unwrap()
        }));

        let proxy_0_user_0_provider_clone = proxy_0_user_0_provider.clone();
        handles.push(tokio::spawn(async move {
            proxy_0_user_0_provider_clone
                .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
                .await
                .unwrap()
                .unwrap()
        }));

        // send 1 to proxy 1 user 0
        let proxy_1_user_0_provider_clone = proxy_1_user_0_provider.clone();
        handles.push(tokio::spawn(async move {
            proxy_1_user_0_provider_clone
                .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
                .await
                .unwrap()
                .unwrap()
        }));
    }

    try_join_all(handles).await.unwrap();

    // Flush all stats here
    // TODO: the test should maybe pause time so that stats definitely flush from our queries.
    let flush_0_count = x_0.flush_stats().await.unwrap();
    assert_eq!(flush_0_count.timeseries, 0);
    assert_eq!(flush_0_count.relational, 1);

    let flush_1_count = x_1.flush_stats().await.unwrap();
    assert_eq!(flush_1_count.timeseries, 0);
    assert_eq!(flush_1_count.relational, 1);

    todo!("Need to validate all the stat accounting now");
}
