use crate::balance::Balance;
use crate::frontend::authorization::{AuthorizationChecks, RpcSecretKey};
use moka::future::Cache;
use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::RwLock as AsyncRwLock;

/// Cache data from the database about rpc keys
pub type RpcSecretKeyCache = Cache<RpcSecretKey, AuthorizationChecks>;
/// Cache data from the database about user balances
pub type UserBalanceCache = Cache<u64, Arc<AsyncRwLock<Balance>>>;

#[derive(Clone, Copy, Hash, Eq, PartialEq)]
pub struct RegisteredUserRateLimitKey(pub u64, pub IpAddr);

impl std::fmt::Display for RegisteredUserRateLimitKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.0, self.1)
    }
}
