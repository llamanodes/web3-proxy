use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // TODO: also alter the index to include the BlockHash? or
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
                            .big_integer()
                            .unsigned()
                            .not_null(),
                    )
                    .add_column(
                        ColumnDef::new(IncreaseOnChainBalanceReceipt::TokenAddress)
                            .string()
                            .not_null(),
                    )
                    .drop_foreign_key(Alias::new("fk-deposit_to_user_id"))
                    .add_foreign_key(
                        TableForeignKey::new()
                            .name("fk-deposit_to_user_id-v2")
                            .from_col(IncreaseOnChainBalanceReceipt::DepositToUserId)
                            .to_tbl(User::Table)
                            .to_col(User::Id),
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
    DepositToUserId,
}

#[derive(Iden)]
enum User {
    Table,
    Id,
}
