mod common;

use crate::common::{
    admin_increases_balance::admin_increase_balance,
    create_admin::create_user_as_admin,
    create_user::{create_user, set_user_tier},
    rpc_key::user_get_provider,
    user_balance::user_get_balance,
    TestApp,
};
use ethers::prelude::U64;
use migration::sea_orm::prelude::Decimal;
use std::time::Duration;
use tracing::info;
use web3_proxy::balance::Balance;

#[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[test_log::test(tokio::test)]
async fn test_sum_credits_used() {
    // chain_id 999_001_999 costs $.10/CU
    let x = TestApp::spawn(999_001_999, true).await;

    let r = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();

    // create wallets for users
    let user_wallet = x.wallet(0);
    let admin_wallet = x.wallet(1);

    // log in to create users
    let admin_login_response = create_user_as_admin(&x, &r, &admin_wallet).await;
    let user_login_response = create_user(&x, &r, &user_wallet, None).await;

    set_user_tier(&x, user_login_response.user.clone(), "Premium")
        .await
        .unwrap();

    // TODO: set the user's user_id to the "Premium" tier

    info!("starting balance");
    let balance: Balance = user_get_balance(&x, &r, &user_login_response).await;
    assert_eq!(
        balance.total_frontend_requests, 0,
        "total_frontend_requests"
    );
    assert_eq!(balance.total_cache_misses, 0, "total_cache_misses");
    assert_eq!(
        balance.total_spent_paid_credits,
        0.into(),
        "total_spent_paid_credits"
    );
    assert_eq!(balance.total_spent, 0.into(), "total_spent");
    assert_eq!(balance.remaining(), 0.into(), "remaining");
    assert!(!balance.active_premium(), "active_premium");
    assert!(!balance.was_ever_premium(), "was_ever_premium");

    info!("make one free request against the public RPC of 16 CU");
    x.proxy_provider
        .request::<_, Option<U64>>("eth_blockNumber", ())
        .await
        .unwrap();

    // connect to the user's rpc url
    let user_proxy_provider = user_get_provider(&x, &r, &user_login_response)
        .await
        .unwrap();

    info!("make one cached authenticated (but out of funds) rpc request of 16 CU");
    user_proxy_provider
        .request::<_, Option<U64>>("eth_blockNumber", ())
        .await
        .unwrap();

    let query_cost: Decimal = "1.00".parse().unwrap();

    // let archive_multiplier: Decimal = "2.5".parse().unwrap();

    let cache_multipler: Decimal = "0.75".parse().unwrap();

    let cached_query_cost: Decimal = query_cost * cache_multipler;

    // flush stats
    let flushed = x.flush_stats().await.unwrap();
    assert_eq!(flushed.relational, 2, "relational");
    assert_eq!(flushed.timeseries, 0, "timeseries");

    // Give user wallet $1000
    admin_increase_balance(&x, &r, &admin_login_response, &user_wallet, 1000.into()).await;

    // check balance
    let balance: Balance = user_get_balance(&x, &r, &user_login_response).await;
    assert_eq!(
        balance.total_frontend_requests, 1,
        "total_frontend_requests"
    );
    assert_eq!(balance.total_cache_misses, 0, "total_cache_misses");
    assert_eq!(
        balance.total_spent_paid_credits,
        0.into(),
        "total_spent_paid_credits"
    );
    assert_eq!(balance.total_spent, cached_query_cost, "total_spent"); // TODO: not sure what this should be
    assert_eq!(balance.remaining(), 1000.into(), "remaining");
    assert!(balance.active_premium(), "active_premium");
    assert!(balance.was_ever_premium(), "was_ever_premium");

    info!("make one cached authenticated rpc request of 16 CU");
    user_proxy_provider
        .request::<_, Option<U64>>("eth_blockNumber", ())
        .await
        .unwrap();

    // flush stats
    let flushed = x.flush_stats().await.unwrap();
    assert_eq!(flushed.relational, 1);

    // check balance
    let balance: Balance = user_get_balance(&x, &r, &user_login_response).await;
    assert_eq!(
        balance.total_frontend_requests, 2,
        "total_frontend_requests"
    );
    assert_eq!(balance.total_cache_misses, 0, "total_cache_misses");
    assert_eq!(
        balance.total_spent,
        cached_query_cost * Decimal::from(2),
        "total_spent"
    );
    assert_eq!(
        balance.total_spent_paid_credits, cached_query_cost,
        "total_spent_paid_credits"
    );
    assert_eq!(
        balance.remaining(),
        Decimal::from(1000) - cached_query_cost,
        "remaining"
    );
    assert!(balance.active_premium(), "active_premium");
    assert!(balance.was_ever_premium(), "was_ever_premium");

    info!("make ten cached authenticated requests of 16 CU");
    for _ in 0..10 {
        user_proxy_provider
            .request::<_, Option<U64>>("eth_blockNumber", ())
            .await
            .unwrap();
    }

    // flush stats
    let flushed = x.flush_stats().await.unwrap();
    assert_eq!(flushed.relational, 1);

    // check balance
    info!("checking the final balance");
    let balance: Balance = user_get_balance(&x, &r, &user_login_response).await;

    // the first of our 12 total requests request was on the free tier, so paid_credits should only count 11
    let expected_total_spent_paid_credits = Decimal::from(11) * cached_query_cost;

    assert_eq!(
        balance.total_frontend_requests, 12,
        "total_frontend_requests"
    );
    assert_eq!(balance.total_cache_misses, 0, "total_cache_misses");
    assert_eq!(
        balance.total_spent_paid_credits, expected_total_spent_paid_credits,
        "total_spent_paid_credits"
    );
    assert_eq!(
        balance.total_spent,
        expected_total_spent_paid_credits + cached_query_cost,
        "total_spent"
    );
    assert_eq!(
        balance.remaining(),
        Decimal::from(1000) - expected_total_spent_paid_credits
    );
    assert!(balance.active_premium(), "active_premium");
    assert!(balance.was_ever_premium(), "was_ever_premium");

    // TODO: make enough queries to push the user balance negative

    // check admin's balance to make sure nothing is leaking
    info!("checking the admin");
    let admin_balance: Balance = user_get_balance(&x, &r, &admin_login_response).await;

    assert!(!admin_balance.active_premium(), "active_premium");
    assert!(!admin_balance.was_ever_premium(), "was_ever_premium");
    assert_eq!(
        admin_balance.total_frontend_requests, 0,
        "total_frontend_requests"
    );
    assert_eq!(admin_balance.total_cache_misses, 0, "total_cache_misses");
    assert_eq!(
        admin_balance.total_spent_paid_credits,
        0.into(),
        "total_spent_paid_credits"
    );
    assert_eq!(admin_balance.total_spent, 0.into(), "total_spent");
    assert_eq!(admin_balance.remaining(), 0.into(), "remaining");

    // TODO: query "user 0" to get the public counts
}
