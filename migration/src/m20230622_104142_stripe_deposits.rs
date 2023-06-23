use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Replace the sample below with your own migration scripts
        manager
            .create_table(
                Table::create()
                    .table(StripeIncreaseBalanceReceipt::Table)
                    .col(
                        ColumnDef::new(StripeIncreaseBalanceReceipt::Id)
                            .big_unsigned()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(StripeIncreaseBalanceReceipt::StripePaymentIntendId)
                            .string()
                            .not_null(),
                    )
                    .col(
                        // I will not mark this as "not null", so we can track who to refund more easily if needed
                        ColumnDef::new(StripeIncreaseBalanceReceipt::DepositToUserId)
                            .big_unsigned(),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk-stripe_deposits_to_user_id")
                            .from(
                                StripeIncreaseBalanceReceipt::Table,
                                StripeIncreaseBalanceReceipt::DepositToUserId,
                            )
                            .to(User::Table, User::Id),
                    )
                    .col(
                        ColumnDef::new(StripeIncreaseBalanceReceipt::Amount)
                            .decimal_len(20, 10)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(StripeIncreaseBalanceReceipt::Currency)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(StripeIncreaseBalanceReceipt::Status)
                            .string()
                            .not_null(),
                    )
                    .col(ColumnDef::new(StripeIncreaseBalanceReceipt::Description).string())
                    .col(
                        ColumnDef::new(StripeIncreaseBalanceReceipt::DateCreated)
                            .timestamp()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(
                Table::drop()
                    .table(StripeIncreaseBalanceReceipt::Table)
                    .to_owned(),
            )
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum User {
    Table,
    Id,
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum StripeIncreaseBalanceReceipt {
    Table,
    Id,
    StripePaymentIntendId,
    Amount,
    Currency,
    Status,
    DepositToUserId,
    Description,
    DateCreated,
}
