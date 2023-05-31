use argh::FromArgs;
use migration::sea_orm::DatabaseConnection;
use web3_proxy::relational_db::{drop_migration_lock, migrate_db};

#[derive(FromArgs, PartialEq, Debug, Eq)]
/// In case of emergency, break glass.
#[argh(subcommand, name = "drop_migration_lock")]
pub struct DropMigrationLockSubCommand {
    #[argh(option)]
    /// run migrations after dropping the lock
    and_migrate: bool,
}

impl DropMigrationLockSubCommand {
    pub async fn main(&self, db_conn: &DatabaseConnection) -> anyhow::Result<()> {
        if self.and_migrate {
            migrate_db(db_conn, true).await?;
        } else {
            // just drop the lock
            drop_migration_lock(db_conn).await?;
        }

        Ok(())
    }
}
