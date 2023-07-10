use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(RpcAccountingV2::Table)
                    .add_column(
                        ColumnDef::new(RpcAccountingV2::SumInclFreeCreditsUsed)
                            .decimal_len(20, 10)
                            .default("0.0")
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                sea_query::Table::alter()
                    .table(RpcAccountingV2::Table)
                    .drop_column(RpcAccountingV2::SumInclFreeCreditsUsed)
                    .to_owned(),
            )
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum RpcAccountingV2 {
    Table,
    SumInclFreeCreditsUsed,
}
