use crate::balance::Balance;
use crate::errors::Web3ProxyResult;
use crate::frontend::authorization::{AuthorizationChecks, RpcSecretKey};
use derive_more::From;
use entities::rpc_key;
use migration::sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use moka::future::{Cache, ConcurrentCacheExt};
use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::RwLock as AsyncRwLock;
use tracing::trace;

/// Cache data from the database about rpc keys
/// TODO: try Ulid/u128 instead of RpcSecretKey in case my hash method is broken
pub type RpcSecretKeyCache = Cache<RpcSecretKey, AuthorizationChecks>;

#[derive(Clone, Copy, Hash, Eq, PartialEq)]
pub struct RegisteredUserRateLimitKey(pub u64, pub IpAddr);

impl std::fmt::Display for RegisteredUserRateLimitKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.0, self.1)
    }
}

/// Cache data from the database about user balances
#[derive(Clone, From)]
pub struct UserBalanceCache(pub Cache<u64, Arc<AsyncRwLock<Balance>>>);

impl UserBalanceCache {
    pub async fn get_or_insert(
        &self,
        db_conn: &DatabaseConnection,
        user_id: u64,
    ) -> Web3ProxyResult<Arc<AsyncRwLock<Balance>>> {
        if user_id == 0 {
            return Ok(Arc::new(AsyncRwLock::new(Balance::default())));
        }

        let x = self
            .0
            .try_get_with(user_id, async move {
                let x = match Balance::try_from_db(db_conn, user_id).await? {
                    None => Balance {
                        user_id,
                        ..Default::default()
                    },
                    Some(x) => x,
                };
                trace!(?x, "from database");

                Ok(Arc::new(AsyncRwLock::new(x)))
            })
            .await?;

        Ok(x)
    }

    pub async fn invalidate(
        &self,
        user_id: &u64,
        db_conn: &DatabaseConnection,
        rpc_secret_key_cache: &RpcSecretKeyCache,
    ) -> Web3ProxyResult<()> {
        self.0.invalidate(user_id).await;

        trace!(%user_id, "invalidating");

        // Remove all RPC-keys owned by this user from the cache, s.t. rate limits are re-calculated
        let rpc_keys = rpc_key::Entity::find()
            .filter(rpc_key::Column::UserId.eq(*user_id))
            .all(db_conn)
            .await?;

        for rpc_key_entity in rpc_keys {
            let rpc_key_id = rpc_key_entity.id;
            let secret_key = rpc_key_entity.secret_key.into();

            trace!(%user_id, %rpc_key_id, ?secret_key, "invalidating");

            rpc_secret_key_cache.invalidate(&secret_key).await;
        }

        Ok(())
    }
}
