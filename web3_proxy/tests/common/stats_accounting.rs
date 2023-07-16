use crate::common::TestApp;
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, trace};
use web3_proxy::frontend::users::authentication::LoginPostResponse;

/// Get the user stats accounting

/// Helper function to get the user's influx stats
#[allow(unused)]
pub async fn user_get_mysql_stats(
    x: &TestApp,
    r: &reqwest::Client,
    login_response: &LoginPostResponse,
) -> serde_json::Value {
    let mysql_stats = format!("{}user/stats/accounting", x.proxy_provider.url());

    let _stats_response = r
        .get(mysql_stats)
        .bearer_auth(login_response.bearer_token)
        .send()
        .await
        .unwrap();
    trace!(
        ?_stats_response,
        "get stats for user #{}",
        login_response.user.id
    );
    assert_eq!(_stats_response.status(), 200);
    let stats_response = _stats_response.json().await.unwrap();
    info!("stats_response: {:#}", json!(&stats_response));
    stats_response
}

/// Helper function to get the user's balance
#[allow(unused)]
pub async fn user_get_influx_stats_detailed(
    x: &TestApp,
    r: &reqwest::Client,
    login_response: &LoginPostResponse,
) -> serde_json::Value {
    let stats_detailed = format!("{}user/stats/detailed", x.proxy_provider.url());

    let _stats_response = r
        .get(stats_detailed)
        .bearer_auth(login_response.bearer_token)
        .send()
        .await
        .unwrap();
    info!(
        ?_stats_response,
        "get stats for user #{}", login_response.user.id
    );
    assert_eq!(_stats_response.status(), 200);
    let stats_response = _stats_response.json().await.unwrap();
    info!("stats_response: {:#}", json!(&stats_response));
    stats_response
}

#[allow(unused)]
pub async fn user_get_influx_stats_aggregated(
    x: &TestApp,
    r: &reqwest::Client,
    login_response: &LoginPostResponse,
    chain_id: u64,
) -> serde_json::Value {
    let query_window_seconds = 300;
    let chain_id = chain_id;
    let start = SystemTime::now();
    let query_start = start
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards")
        .as_secs()
        - 1200;
    let stats_aggregated = format!(
        "{}user/stats/aggregate?query_window_seconds={}&chain_id={}&query_start={}",
        x.proxy_provider.url(),
        query_window_seconds,
        chain_id,
        query_start
    );

    info!("Stats aggregated request is: {:?}", stats_aggregated);

    info!("Sending queries to influx");
    let _stats_response = r
        .get(stats_aggregated)
        .bearer_auth(login_response.bearer_token)
        .send()
        .await
        .unwrap();
    info!(
        ?_stats_response,
        "get stats for user #{}", login_response.user.id
    );
    assert_eq!(_stats_response.status(), 200);
    let stats_response = _stats_response.json().await.unwrap();
    info!("stats_response: {:#}", json!(&stats_response));
    stats_response
}
