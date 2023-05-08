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
                    .table(IncreaseBalanceReceipt::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(IncreaseBalanceReceipt::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(IncreaseBalanceReceipt::TxHash)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(IncreaseBalanceReceipt::ChainId)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(IncreaseBalanceReceipt::Amount)
                            .decimal_len(20, 10)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(IncreaseBalanceReceipt::DepositToUserId)
                            .big_unsigned()
                            .unique_key()
                            .not_null(),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk-deposit_to_user_id")
                            .from(
                                IncreaseBalanceReceipt::Table,
                                IncreaseBalanceReceipt::DepositToUserId,
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
                    .name("idx-increase_balance_receipt-unique-chain_id-tx_hash")
                    .table(IncreaseBalanceReceipt::Table)
                    .col(IncreaseBalanceReceipt::ChainId)
                    .col(IncreaseBalanceReceipt::TxHash)
                    .unique()
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Replace the sample below with your own migration scripts
        manager
            .drop_table(
                Table::drop()
                    .table(IncreaseBalanceReceipt::Table)
                    .to_owned(),
            )
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum IncreaseBalanceReceipt {
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
