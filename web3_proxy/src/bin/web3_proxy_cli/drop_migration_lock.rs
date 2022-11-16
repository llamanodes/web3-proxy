use argh::FromArgs;
use migration::sea_orm::DatabaseConnection;
use web3_proxy::app::drop_migration_lock;

#[derive(FromArgs, PartialEq, Debug, Eq)]
/// In case of emergency, break glass.
#[argh(subcommand, name = "drop_migration_lock")]
pub struct DropMigrationLockSubCommand {}

impl DropMigrationLockSubCommand {
    pub async fn main(&self, db_conn: &DatabaseConnection) -> anyhow::Result<()> {
        drop_migration_lock(db_conn).await?;

        Ok(())
    }
}
