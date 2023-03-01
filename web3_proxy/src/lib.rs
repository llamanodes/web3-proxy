pub mod app;
pub mod app_stats;
pub mod admin_queries;
pub mod atomics;
pub mod block_number;
pub mod config;
pub mod frontend;
pub mod jsonrpc;
pub mod metrics_frontend;
pub mod pagerduty;
pub mod rpcs;
pub mod user_queries;
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
}
