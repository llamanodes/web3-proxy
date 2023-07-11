mod common;
use web3_proxy::{balance::Balance, rpcs::blockchain::ArcBlock};

use crate::common::{
    admin_increases_balance::admin_increase_balance, create_admin::create_user_as_admin,
    create_user::create_user, user_balance::user_get_balance, TestApp,
};
use migration::sea_orm::prelude::Decimal;
use std::time::Duration;

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

    // TODO: set the user's user_id to the "Premium" tier

    let balance: Balance = user_get_balance(&x, &r, &user_login_response).await;
    assert_eq!(balance.total_frontend_requests, 0);
    assert_eq!(balance.total_cache_misses, 0);
    assert_eq!(balance.total_spent_paid_credits, 0.into());
    assert_eq!(balance.total_spent, 0.into());
    assert_eq!(balance.remaining(), 0.into());
    assert!(!balance.active_premium());
    assert!(!balance.was_ever_premium());

    // make one free request of 16 CU
    x.proxy_provider
        .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
        .await
        .unwrap();

    let query_cost: Decimal = "1.60".parse().unwrap();

    let cache_multipler: Decimal = "0.75".parse().unwrap();

    let cached_query_cost: Decimal = query_cost * cache_multipler;

    // flush stats
    let flushed = x.flush_stats().await.unwrap();
    assert_eq!(flushed.relational, 1);

    // Give user wallet $1000
    admin_increase_balance(&x, &r, &admin_login_response, &user_wallet, 1000.into()).await;

    // check balance
    let balance: Balance = user_get_balance(&x, &r, &user_login_response).await;
    assert_eq!(balance.total_frontend_requests, 1);
    assert_eq!(balance.total_cache_misses, 1);
    assert_eq!(balance.total_spent_paid_credits, 0.into());
    assert_eq!(balance.total_spent, query_cost); // TODO: what should this actually be?
    assert_eq!(balance.remaining(), 1000.into());
    assert!(balance.active_premium());
    assert!(balance.was_ever_premium());

    // make one rpc request of 16 CU
    x.proxy_provider
        .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
        .await
        .unwrap();

    // flush stats
    let flushed = x.flush_stats().await.unwrap();
    assert_eq!(flushed.relational, 1);

    // check balance
    let balance: Balance = user_get_balance(&x, &r, &user_login_response).await;
    assert_eq!(balance.total_frontend_requests, 2);
    assert_eq!(balance.total_cache_misses, 1);
    assert_eq!(balance.total_spent_paid_credits, query_cost);
    assert_eq!(balance.total_spent, query_cost + cached_query_cost);
    assert_eq!(
        balance.remaining(),
        Decimal::from(1000) - query_cost - cached_query_cost
    );
    assert!(balance.active_premium());
    assert!(balance.was_ever_premium());

    // make ten rpc request of 16 CU
    for _ in 0..10 {
        x.proxy_provider
            .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
            .await
            .unwrap();
    }

    // flush stats
    let flushed = x.flush_stats().await.unwrap();
    assert_eq!(flushed.relational, 1);

    // check balance
    let balance: Balance = user_get_balance(&x, &r, &user_login_response).await;

    // the first request was on the free tier
    let expected_total_spent_paid_credits = Decimal::from(11) * cached_query_cost;

    assert_eq!(balance.total_frontend_requests, 12);
    assert_eq!(balance.total_cache_misses, 1);
    assert_eq!(
        balance.total_spent_paid_credits,
        expected_total_spent_paid_credits
    );
    assert_eq!(
        balance.total_spent,
        expected_total_spent_paid_credits - cached_query_cost
    );
    assert_eq!(
        balance.remaining(),
        Decimal::from(1000) - expected_total_spent_paid_credits
    );
    assert!(balance.active_premium());
    assert!(balance.was_ever_premium());

    // TODO: make enough queries to push the user balance negative

    // check admin's balance to make sure nothing is leaking
    let admin_balance: Balance = user_get_balance(&x, &r, &user_login_response).await;
    assert_eq!(admin_balance.remaining(), 0.into());
    assert!(!admin_balance.active_premium());
    assert!(!admin_balance.was_ever_premium());
    assert_eq!(admin_balance.total_frontend_requests, 0);
    assert_eq!(admin_balance.total_cache_misses, 0);
    assert_eq!(admin_balance.total_spent_paid_credits, 0.into());
    assert_eq!(admin_balance.total_spent, 0.into());
}
