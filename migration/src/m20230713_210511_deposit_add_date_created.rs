use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Add column to both tables
        manager
            .alter_table(
                Table::alter()
                    .table(AdminIncreaseBalanceReceipt::Table)
                    .add_column(
                        ColumnDef::new(AdminIncreaseBalanceReceipt::DateCreated)
                            .timestamp()
                            .extra("DEFAULT CURRENT_TIMESTAMP".to_string())
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;
        // Add column to both tables
        manager
            .alter_table(
                Table::alter()
                    .table(IncreaseOnChainBalanceReceipt::Table)
                    .add_column(
                        ColumnDef::new(IncreaseOnChainBalanceReceipt::DateCreated)
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
            .table(AdminIncreaseBalanceReceipt::Table)
            .values([(AdminIncreaseBalanceReceipt::DateCreated, Some(now).into())])
            .to_owned();
        manager.exec_stmt(update_to_current_timestamp).await?;
        // Then change all columns to "now"
        let update_to_current_timestamp = Query::update()
            .table(IncreaseOnChainBalanceReceipt::Table)
            .values([(IncreaseOnChainBalanceReceipt::DateCreated, Some(now).into())])
            .to_owned();
        manager.exec_stmt(update_to_current_timestamp).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Replace the sample below with your own migration scripts
        manager
            .alter_table(
                Table::alter()
                    .table(AdminIncreaseBalanceReceipt::Table)
                    .drop_column(AdminIncreaseBalanceReceipt::DateCreated)
                    .to_owned(),
            )
            .await?;
        // Add column to both tables
        manager
            .alter_table(
                Table::alter()
                    .table(IncreaseOnChainBalanceReceipt::Table)
                    .drop_column(IncreaseOnChainBalanceReceipt::DateCreated)
                    .to_owned(),
            )
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum AdminIncreaseBalanceReceipt {
    Table,
    DateCreated,
}

#[derive(Iden)]
enum IncreaseOnChainBalanceReceipt {
    Table,
    DateCreated,
}
