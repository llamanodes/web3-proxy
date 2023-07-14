use crate::{errors::Web3ProxyError, relational_db::DatabaseReplica};
use derivative::Derivative;
use migration::{
    sea_orm::{DatabaseConnection, DatabaseTransaction, TransactionTrait},
    DbErr,
};
use std::sync::{Arc, LazyLock};
use tokio::sync::RwLock as AsyncRwLock;

pub static DB_CONN: LazyLock<AsyncRwLock<Result<DatabaseConnection, DatabaseError>>> =
    LazyLock::new(|| AsyncRwLock::new(Err(DatabaseError::NotConfigured)));

pub static DB_REPLICA: LazyLock<AsyncRwLock<Result<DatabaseReplica, DatabaseError>>> =
    LazyLock::new(|| AsyncRwLock::new(Err(DatabaseError::NotConfigured)));

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

#[inline]
pub async fn global_db_conn() -> Result<DatabaseConnection, DatabaseError> {
    DB_CONN.read().await.clone()
}

#[inline]
pub async fn global_db_transaction() -> Result<DatabaseTransaction, DatabaseError> {
    let x = global_db_conn().await?;

    let x = x
        .begin()
        .await
        .map_err(|x| DatabaseError::Begin(Arc::new(x)))?;

    Ok(x)
}

#[inline]
pub async fn global_db_replica_conn() -> Result<DatabaseReplica, DatabaseError> {
    DB_REPLICA.read().await.clone()
}
