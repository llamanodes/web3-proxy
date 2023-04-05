use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Add a nullable timestamp column to check if things were migrated in the rpc_accounting table
        manager
            .alter_table(
                Table::alter()
                    .table(RpcAccounting::Table)
                    .add_column(ColumnDef::new(RpcAccounting::Migrated).timestamp())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(RpcAccounting::Table)
                    .drop_column(RpcAccounting::Migrated)
                    .to_owned(),
            )
            .await
    }
}

/// partial table for RpcAccounting
#[derive(Iden)]
enum RpcAccounting {
    Table,
    Migrated,
}
