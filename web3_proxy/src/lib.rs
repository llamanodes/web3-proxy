#![feature(trait_alias)]

pub mod admin_queries;
pub mod app;
pub mod block_number;
pub mod config;
pub mod errors;
pub mod frontend;
pub mod http_params;
pub mod jsonrpc;
pub mod pagerduty;
pub mod prometheus;
pub mod referral_code;
pub mod relational_db;
pub mod response_cache;
pub mod rpcs;
pub mod stats;
pub mod user_token;

use serde::Deserialize;

// Push some commonly used types here. Can establish a folder later on
/// Query params for our `post_login` handler.
#[derive(Debug, Deserialize)]
pub struct PostLoginQuery {
    /// While we are in alpha/beta, we require users to supply an invite code.
    /// The invite code (if any) is set in the application's config.
    /// This may eventually provide some sort of referral bonus.
    invite_code: Option<String>,
}

/// JSON body to our `post_login` handler.
/// Currently only siwe logins that send an address, msg, and sig are allowed.
/// Email/password and other login methods are planned.
#[derive(Debug, Deserialize)]
pub struct PostLogin {
    sig: String,
    msg: String,
    pub referral_code: Option<String>,
}
