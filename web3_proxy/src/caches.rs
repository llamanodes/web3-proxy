use crate::frontend::authorization::{AuthorizationChecks, Balance, RpcSecretKey};
use moka::future::Cache;
use parking_lot::RwLock;
use std::fmt;
use std::net::IpAddr;
use std::num::NonZeroU64;
use std::sync::Arc;

/// Cache data from the database about rpc keys
pub type RpcSecretKeyCache = Cache<RpcSecretKey, AuthorizationChecks>;
/// Cache data from the database about user balances
pub type UserBalanceCache = Cache<NonZeroU64, Arc<RwLock<Balance>>>;

#[derive(Clone, Copy, Hash, Eq, PartialEq)]
pub struct RegisteredUserRateLimitKey(pub u64, pub IpAddr);

impl std::fmt::Display for RegisteredUserRateLimitKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.0, self.1)
    }
}
