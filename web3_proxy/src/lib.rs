#![feature(lazy_cell)]
#![feature(let_chains)]
#![feature(trait_alias)]
#![feature(result_flattening)]
#![forbid(unsafe_code)]

pub mod admin_queries;
pub mod app;
pub mod balance;
pub mod block_number;
pub mod caches;
pub mod compute_units;
pub mod config;
pub mod errors;
pub mod frontend;
pub mod globals;
pub mod http_params;
pub mod jsonrpc;
pub mod pagerduty;
pub mod prelude;
pub mod premium;
pub mod prometheus;
pub mod referral_code;
pub mod relational_db;
pub mod response_cache;
pub mod rpcs;
pub mod secrets;
pub mod stats;
pub mod test_utils;
pub mod user_token;

#[cfg(feature = "rdkafka")]
pub mod kafka;
