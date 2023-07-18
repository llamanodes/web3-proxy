mod common;

use crate::common::create_provider_with_rpc_key::create_provider_for_user;
use crate::common::influx::TestInflux;
use crate::common::rpc_key::user_get_first_rpc_key;
use crate::common::stats_accounting::{user_get_influx_stats_aggregated, user_get_mysql_stats};
use crate::common::user_balance::user_get_balance;
use crate::common::{
    admin_increases_balance::admin_increase_balance, anvil::TestAnvil,
    create_admin::create_user_as_admin, create_user::create_user, mysql::TestMysql, TestApp,
};
use futures::future::try_join_all;
use rust_decimal::Decimal;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{info, warn};
use web3_proxy::rpcs::blockchain::ArcBlock;

#[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[test_log::test(tokio::test)]
async fn test_multiple_proxies_stats_add_up() {
    let chain_id = 999_001_999;
    let a = TestAnvil::spawn(chain_id).await;

    let db = TestMysql::spawn().await;

    let influx = TestInflux::spawn().await;

    let r = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap();

    // Since when do indices start with 1
    let x_0 = TestApp::spawn(&a, Some(&db), Some(&influx)).await;
    let x_1 = TestApp::spawn(&a, Some(&db), Some(&influx)).await;

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

    warn!("Created users, generated providers");

    info!("Proxy 1: {:?}", proxy_0_user_0_provider);
    info!("Proxy 2: {:?}", proxy_1_user_0_provider);

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
    let flush_1_count = x_1.flush_stats().await.unwrap();

    // Wait a bit
    sleep(Duration::from_secs(5)).await;
    info!("Counts 0 are: {:?}", flush_0_count);
    assert_eq!(flush_0_count.relational, 1);
    assert_eq!(flush_0_count.timeseries, 2);
    info!("Counts 1 are: {:?}", flush_1_count);
    assert_eq!(flush_1_count.relational, 1);
    assert_eq!(flush_1_count.timeseries, 2);

    // get stats now
    // todo!("Need to validate all the stat accounting now");
    // Get the stats from here
    let mysql_stats = user_get_mysql_stats(&x_0, &r, &user_0_login).await;
    info!("mysql stats are: {:?}", mysql_stats);

    let influx_aggregate_stats =
        user_get_influx_stats_aggregated(&x_0, &r, &user_0_login, chain_id).await;
    info!(
        "influx_aggregate_stats stats are: {:?}",
        influx_aggregate_stats
    );

    // Get the balance
    let user_0_balance_post = user_get_balance(&x_0, &r, &user_0_login).await;
    let influx_stats = influx_aggregate_stats["result"].get(0).unwrap();
    let mysql_stats = mysql_stats["stats"].get(0).unwrap();

    assert_eq!(
        user_0_balance_post.total_frontend_requests,
        number_requests * 3
    );

    info!("Influx and mysql stats are");
    info!(?influx_stats);
    info!(?mysql_stats);

    assert_eq!(
        mysql_stats["error_response"],
        influx_stats["error_response"]
    );
    assert_eq!(
        mysql_stats["archive_needed"],
        influx_stats["archive_needed"]
    );
    assert_eq!(
        Decimal::from_str(&mysql_stats["chain_id"].to_string().replace('"', "")).unwrap(),
        Decimal::from_str(&influx_stats["chain_id"].to_string().replace('"', "")).unwrap()
    );
    assert_eq!(
        Decimal::from_str(&mysql_stats["no_servers"].to_string()).unwrap(),
        Decimal::from_str(&influx_stats["no_servers"].to_string()).unwrap()
    );
    assert_eq!(
        Decimal::from_str(&mysql_stats["cache_hits"].to_string()).unwrap(),
        Decimal::from_str(&influx_stats["total_cache_hits"].to_string()).unwrap()
    );
    assert_eq!(
        Decimal::from_str(&mysql_stats["cache_misses"].to_string()).unwrap(),
        Decimal::from_str(&influx_stats["total_cache_misses"].to_string()).unwrap()
    );
    assert_eq!(
        Decimal::from_str(&mysql_stats["frontend_requests"].to_string()).unwrap(),
        Decimal::from_str(&influx_stats["total_frontend_requests"].to_string()).unwrap()
    );
    assert_eq!(
        Decimal::from_str(&mysql_stats["sum_credits_used"].to_string().replace('"', "")).unwrap(),
        Decimal::from_str(
            &influx_stats["total_credits_used"]
                .to_string()
                .replace('"', "")
        )
        .unwrap()
    );
    assert_eq!(
        Decimal::from_str(
            &mysql_stats["sum_incl_free_credits_used"]
                .to_string()
                .replace('"', "")
        )
        .unwrap(),
        Decimal::from_str(
            &influx_stats["total_incl_free_credits_used"]
                .to_string()
                .replace('"', "")
        )
        .unwrap()
    );
    assert_eq!(
        Decimal::from_str(&mysql_stats["sum_request_bytes"].to_string()).unwrap(),
        Decimal::from_str(&influx_stats["total_request_bytes"].to_string()).unwrap()
    );
    assert_eq!(
        Decimal::from_str(&mysql_stats["sum_response_bytes"].to_string()).unwrap(),
        Decimal::from_str(
            &influx_stats["total_response_bytes"]
                .to_string()
                .replace('"', "")
        )
        .unwrap()
    );
    assert_eq!(
        Decimal::from_str(&mysql_stats["sum_response_millis"].to_string()).unwrap(),
        Decimal::from_str(
            &influx_stats["total_response_millis"]
                .to_string()
                .replace('"', "")
        )
        .unwrap()
    );
    assert_eq!(
        Decimal::from_str(&mysql_stats["backend_requests"].to_string()).unwrap(),
        Decimal::from_str(&influx_stats["total_backend_requests"].to_string()).unwrap()
    );

    // We don't have gauges so we cant really fix this in influx. will get back to this later
    // assert_eq!(
    //     Decimal::from(user_0_balance_post.remaining()),
    //     Decimal::from_str(&influx_stats["balance"].to_string()).unwrap()
    // );

    // The fields we skip for mysql
    // backend_retries, id, no_servers, period_datetime, rpc_key_id

    // The fields we skip for influx
    // collection, rpc_key, time,

    // let user_get_influx_stats_detailed =
    //     user_get_influx_stats_detailed(&x_0, &r, &user_0_login).await;
    // info!(
    //     "user_get_influx_stats_detailed stats are: {:?}",
    //     user_get_influx_stats_detailed
    // );
}

// Gotta compare stats with influx:
// "stats": [
// {
// "archive_needed": false,
// "backend_requests": 2,
// "backend_retries": 0,
// "cache_hits": 148,
// "cache_misses": 2,
// "chain_id": 999001999,
// "error_response": false,
// "frontend_requests": 150,
// "id": 1,
// "no_servers": 0,
// "period_datetime": "2023-07-13T00:00:00Z",
// "rpc_key_id": 1,
// "sum_credits_used": "180.8000000000",
// "sum_incl_free_credits_used": "180.8000000000",
// "sum_request_bytes": 12433,
// "sum_response_bytes": 230533,
// "sum_response_millis": 194
// }
// ]

// influx
// "result": [
// {
// "archive_needed": false,
// "balance": 939.6,
// "chain_id": "999001999",
// "collection": "opt-in",
// "error_response": false,
// "no_servers": 0,
// "rpc_key": "01H5E9HRZW2S73F1996KPKMYCE",
// "time": "2023-07-16 02:47:56 +00:00",
// "total_backend_requests": 1,
// "total_cache_hits": 49,
// "total_cache_misses": 1,
// "total_credits_used": 60.4,
// "total_frontend_requests": 50,
// "total_incl_free_credits_used": 60.4,
// "total_request_bytes": 4141,
// "total_response_bytes": 76841,
// "total_response_millis": 72
// }
