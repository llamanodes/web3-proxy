use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(AdminIncreaseBalanceReceipt::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(AdminIncreaseBalanceReceipt::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(AdminIncreaseBalanceReceipt::Amount)
                            .decimal_len(20, 10)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AdminIncreaseBalanceReceipt::AdminId)
                            .big_unsigned()
                            .not_null(),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk-admin_id")
                            .from(
                                AdminIncreaseBalanceReceipt::Table,
                                AdminIncreaseBalanceReceipt::AdminId,
                            )
                            .to(Admin::Table, Admin::Id),
                    )
                    .col(
                        ColumnDef::new(AdminIncreaseBalanceReceipt::DepositToUserId)
                            .big_unsigned()
                            .not_null(),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk-admin_deposits_to_user_id")
                            .from(
                                AdminIncreaseBalanceReceipt::Table,
                                AdminIncreaseBalanceReceipt::DepositToUserId,
                            )
                            .to(User::Table, User::Id),
                    )
                    .col(
                        ColumnDef::new(AdminIncreaseBalanceReceipt::Note)
                            .string()
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
                    .table(AdminIncreaseBalanceReceipt::Table)
                    .to_owned(),
            )
            .await
    }
}

#[derive(Iden)]
enum Admin {
    Table,
    Id,
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum User {
    Table,
    Id,
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum AdminIncreaseBalanceReceipt {
    Table,
    Id,
    Amount,
    AdminId,
    DepositToUserId,
    Note,
}
