use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Balance::Table)
                    .add_column(
                        ColumnDef::new(Balance::TotalSpentOutsideFreeTier)
                            .decimal_len(20, 10)
                            .not_null()
                            .default(0.0),
                    )
                    .add_column(
                        ColumnDef::new(Balance::TotalDeposits)
                            .decimal_len(20, 10)
                            .not_null()
                            .default(0.0),
                    )
                    .rename_column(Balance::UsedBalance, Balance::TotalSpentIncludingFreeTier)
                    .drop_column(Balance::AvailableBalance)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Remove the column again I suppose, but this will delete data, needless to say
        manager
            .alter_table(
                Table::alter()
                    .table(Balance::Table)
                    .rename_column(Balance::TotalSpentIncludingFreeTier, Balance::UsedBalance)
                    .drop_column(Balance::TotalSpentOutsideFreeTier)
                    .drop_column(Balance::TotalDeposits)
                    .add_column(
                        ColumnDef::new(Balance::AvailableBalance)
                            .decimal_len(20, 10)
                            .not_null()
                            .default(0.0),
                    )
                    .add_column(
                        ColumnDef::new(Balance::UsedBalance)
                            .decimal_len(20, 10)
                            .not_null()
                            .default(0.0),
                    )
                    .to_owned(),
            )
            .await
    }
}

#[derive(Iden)]
enum Balance {
    Table,
    TotalSpentIncludingFreeTier,
    TotalSpentOutsideFreeTier,
    TotalDeposits,
    AvailableBalance,
    UsedBalance,
}
