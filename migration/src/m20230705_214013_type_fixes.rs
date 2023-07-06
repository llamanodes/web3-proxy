use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Replace the sample below with your own migration scripts
        manager
            .alter_table(
                Table::alter()
                    .table(IncreaseOnChainBalanceReceipt::Table)
                    .modify_column(
                        ColumnDef::new(IncreaseOnChainBalanceReceipt::LogIndex)
                            .big_unsigned()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(IncreaseOnChainBalanceReceipt::Table)
                    .modify_column(
                        ColumnDef::new(IncreaseOnChainBalanceReceipt::LogIndex)
                            .big_integer()
                            .unsigned()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum IncreaseOnChainBalanceReceipt {
    Table,
    LogIndex,
}
