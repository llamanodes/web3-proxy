use sea_orm_migration::prelude::table::ColumnDef;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // add a field to the UserKeys table
        manager
            .alter_table(
                sea_query::Table::alter()
                    .table(RevertLogs::Table)
                    // add column for a better version of rate limiting
                    .add_column(
                        ColumnDef::new(RevertLogs::ChainId)
                            .big_unsigned()
                            .not_null()
                            // create it with a default of 1
                            .default(1),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .alter_table(
                sea_query::Table::alter()
                    .table(RevertLogs::Table)
                    // remove the default
                    .modify_column(
                        ColumnDef::new(RevertLogs::ChainId)
                            .big_unsigned()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // put the RevertLogs back to how it was before our migrations
        manager
            .alter_table(
                sea_query::Table::alter()
                    .table(RevertLogs::Table)
                    .drop_column(RevertLogs::ChainId)
                    .to_owned(),
            )
            .await
    }
}

#[derive(Iden)]
enum RevertLogs {
    Table,
    // Id,
    // UserKeyId,
    // Method,
    // CallData,
    // To,
    // Timestamp,
    ChainId,
}
