// TODO: Try to re-export timestamp from within sea-orm
use chrono;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(StripeIncreaseBalanceReceipt::Table)
                    .modify_column(
                        ColumnDef::new(StripeIncreaseBalanceReceipt::DateCreated)
                            .timestamp()
                            .extra("DEFAULT CURRENT_TIMESTAMP".to_string())
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;

        let now = chrono::offset::Utc::now();

        // Then change all columns to "now"
        let update_to_current_timestamp = Query::update()
            .table(StripeIncreaseBalanceReceipt::Table)
            .values([(StripeIncreaseBalanceReceipt::DateCreated, Some(now).into())])
            .to_owned();

        manager.exec_stmt(update_to_current_timestamp).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Do nothing ...
        Ok(())
    }
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
