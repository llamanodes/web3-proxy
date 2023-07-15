use crate::common::TestApp;
use crate::LoginPostResponse;

/// Get the user stats accounting

/// Helper function to get the user's influx stats
// #[allow(unused)]
// pub async fn user_get_stats(
//     x: &TestApp,
//     r: &reqwest::Client,
//     login_response: &LoginPostResponse,
// ) -> Stats {
//     let stats_aggregated = format!("{}user/stats/aggregate", x.proxy_provider.url());
//     let stats_detailed = format!("{}user/stats/detailed", x.proxy_provider.url());
// }
//
// /// Helper function to get the user's balance
// #[allow(unused)]
// pub async fn user_get_influx_stats(
//     x: &TestApp,
//     r: &reqwest::Client,
//     login_response: &LoginPostResponse,
// ) -> Stats {
// }
