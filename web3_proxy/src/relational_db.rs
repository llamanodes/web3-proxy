use anyhow::Context;
use derive_more::From;
use migration::sea_orm::{self, ConnectionTrait, Database};
use migration::sea_query::table::ColumnDef;
use migration::{Alias, DbErr, Migrator, MigratorTrait, Table};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info, warn};

pub use migration::sea_orm::DatabaseConnection;

/// Simple wrapper so that we can keep track of read only connections.
/// WARNING! This does not actually block writing in the compiler!
/// There will be runtime errors if this is used to write though.
#[derive(Clone, From)]
pub struct DatabaseReplica(DatabaseConnection);

// TODO: this still doesn't work like i want. I want to be able to do `query.one(&DatabaseReplicate).await?`
impl AsRef<DatabaseConnection> for DatabaseReplica {
    fn as_ref(&self) -> &DatabaseConnection {
        &self.0
    }
}

pub async fn get_db(
    db_url: String,
    min_connections: u32,
    max_connections: u32,
) -> Result<DatabaseConnection, DbErr> {
    // TODO: scrub credentials and then include the db_url in logs
    info!("Connecting to db");

    let mut db_opt = sea_orm::ConnectOptions::new(db_url);

    // TODO: load all these options from the config file. i think docker mysql default max is 100
    // TODO: sqlx info logging is way too verbose for production.
    db_opt
        .acquire_timeout(Duration::from_secs(5))
        .connect_timeout(Duration::from_secs(5))
        .min_connections(min_connections)
        .max_connections(max_connections)
        .sqlx_logging_level(tracing::log::LevelFilter::Trace)
        .sqlx_logging(true);

    Database::connect(db_opt).await
}

pub async fn drop_migration_lock(db_conn: &DatabaseConnection) -> anyhow::Result<()> {
    let db_backend = db_conn.get_database_backend();

    let drop_lock_statment = db_backend.build(Table::drop().table(Alias::new("migration_lock")));

    db_conn
        .execute(drop_lock_statment)
        .await
        .context("dropping lock")?;

    debug!("migration lock unlocked");

    Ok(())
}

/// Be super careful with override_existing_lock! It is very important that only one process is running the migrations at a time!
pub async fn migrate_db(
    db_conn: &DatabaseConnection,
    override_existing_lock: bool,
) -> anyhow::Result<()> {
    let db_backend = db_conn.get_database_backend();

    // TODO: put the timestamp and hostname into this as columns?
    let create_lock_statment = db_backend.build(
        Table::create()
            .table(Alias::new("migration_lock"))
            .col(ColumnDef::new(Alias::new("locked")).boolean().default(true)),
    );

    loop {
        if Migrator::get_pending_migrations(db_conn).await?.is_empty() {
            info!("no migrations to apply");
            return Ok(());
        }

        info!("waiting for migration lock...");

        // there are migrations to apply
        // acquire a lock
        if let Err(err) = db_conn.execute(create_lock_statment.clone()).await {
            if override_existing_lock {
                warn!("OVERRIDING EXISTING LOCK in 10 seconds! ctrl+c now if other migrations are actually running!");

                sleep(Duration::from_secs(10)).await
            } else {
                debug!("Unable to acquire lock. if you are positive no migration is running, run \"web3_proxy_cli drop_migration_lock\". err={:?}", err);

                // TODO: exponential backoff with jitter?
                sleep(Duration::from_secs(1)).await;

                continue;
            }
        }

        debug!("migration lock acquired");
        break;
    }

    info!("migrating...");

    let migration_result = Migrator::up(db_conn, None).await;

    // drop the distributed lock
    drop_migration_lock(db_conn).await?;

    // return if migrations erred
    migration_result.map_err(Into::into)
}

/// Connect to the database and run migrations
pub async fn get_migrated_db(
    db_url: String,
    min_connections: u32,
    max_connections: u32,
) -> anyhow::Result<DatabaseConnection> {
    // TODO: this seems to fail silently
    let db_conn = get_db(db_url, min_connections, max_connections)
        .await
        .context("getting db")?;

    migrate_db(&db_conn, false).await.context("migrating db")?;

    Ok(db_conn)
}
