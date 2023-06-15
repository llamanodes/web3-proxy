use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(IncreaseOnChainBalanceReceipt::Table)
                    .add_column(
                        ColumnDef::new(IncreaseOnChainBalanceReceipt::BlockHash)
                            .string()
                            .not_null(),
                    )
                    .add_column(
                        ColumnDef::new(IncreaseOnChainBalanceReceipt::LogIndex)
                            .integer()
                            .unsigned()
                            .not_null(),
                    )
                    .add_column(
                        ColumnDef::new(IncreaseOnChainBalanceReceipt::TokenAddress)
                            .string()
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
                    .table(Post::Table)
                    .drop_column(IncreaseOnChainBalanceReceipt::BlockHash)
                    .drop_column(IncreaseOnChainBalanceReceipt::LogIndex)
                    .drop_column(IncreaseOnChainBalanceReceipt::TokenAddress)
                    .to_owned(),
            )
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum IncreaseOnChainBalanceReceipt {
    Table,
    BlockHash,
    LogIndex,
    TokenAddress,
}
