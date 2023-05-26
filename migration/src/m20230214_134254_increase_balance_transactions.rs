use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Adds a table which keeps track of which transactions were already added (basically to prevent double spending)
        manager
            .create_table(
                Table::create()
                    .table(IncreaseOnChainBalanceReceipt::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(IncreaseOnChainBalanceReceipt::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(IncreaseOnChainBalanceReceipt::TxHash)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(IncreaseOnChainBalanceReceipt::ChainId)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(IncreaseOnChainBalanceReceipt::Amount)
                            .decimal_len(20, 10)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(IncreaseOnChainBalanceReceipt::DepositToUserId)
                            .big_unsigned()
                            .unique_key()
                            .not_null(),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk-deposit_to_user_id")
                            .from(
                                IncreaseOnChainBalanceReceipt::Table,
                                IncreaseOnChainBalanceReceipt::DepositToUserId,
                            )
                            .to(User::Table, User::Id),
                    )
                    .to_owned(),
            )
            .await?;

        // Add a unique-constraint on chain-id and tx-hash
        manager
            .create_index(
                Index::create()
                    .name("idx-increase_on_chain_balance_receipt-unique-chain_id-tx_hash")
                    .table(IncreaseOnChainBalanceReceipt::Table)
                    .col(IncreaseOnChainBalanceReceipt::ChainId)
                    .col(IncreaseOnChainBalanceReceipt::TxHash)
                    .unique()
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(
                Table::drop()
                    .table(IncreaseOnChainBalanceReceipt::Table)
                    .to_owned(),
            )
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum IncreaseOnChainBalanceReceipt {
    Table,
    Id,
    TxHash,
    ChainId,
    Amount,
    DepositToUserId,
}

#[derive(Iden)]
enum User {
    Table,
    Id,
}
