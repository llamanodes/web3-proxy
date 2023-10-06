// TODO: think a lot more about this

use crate::{app::Web3ProxyApp, errors::Web3ProxyError, relational_db::DatabaseReplica};
use derivative::Derivative;
use migration::{
    sea_orm::{DatabaseConnection, DatabaseTransaction, TransactionTrait},
    DbErr,
};
use parking_lot::RwLock;
use std::sync::{Arc, LazyLock, OnceLock};

pub static APP: OnceLock<Arc<Web3ProxyApp>> = OnceLock::new();

pub static DB_CONN: LazyLock<RwLock<Result<DatabaseConnection, DatabaseError>>> =
    LazyLock::new(|| RwLock::new(Err(DatabaseError::NotConfigured)));

pub static DB_REPLICA: LazyLock<RwLock<Result<DatabaseReplica, DatabaseError>>> =
    LazyLock::new(|| RwLock::new(Err(DatabaseError::NotConfigured)));

#[derive(Clone, Debug, Derivative)]
pub enum DatabaseError {
    /// no database configured. depending on what you need, this may or may not be a problem
    NotConfigured,
    /// an error that happened when creating the connection pool
    Connect(Arc<DbErr>),
    /// an error that just happened
    Begin(Arc<DbErr>),
}

impl From<DatabaseError> for Web3ProxyError {
    fn from(value: DatabaseError) -> Self {
        match value {
            DatabaseError::NotConfigured => Self::NoDatabaseConfigured,
            DatabaseError::Connect(err) | DatabaseError::Begin(err) => Self::DatabaseArc(err),
        }
    }
}

/// TODO: do we need this clone? should we just do DB_CONN.read() whenever we need a Connection?
#[inline]
pub fn global_db_conn() -> Result<DatabaseConnection, DatabaseError> {
    DB_CONN.read().clone()
}

#[inline]
pub async fn global_db_transaction() -> Result<DatabaseTransaction, DatabaseError> {
    let x = global_db_conn()?;

    let x = x
        .begin()
        .await
        .map_err(|x| DatabaseError::Begin(Arc::new(x)))?;

    Ok(x)
}

/// TODO: do we need this clone?
#[inline]
pub fn global_db_replica_conn() -> Result<DatabaseReplica, DatabaseError> {
    DB_REPLICA.read().clone()
}
