use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Track spend inside the RPC accounting v2 table
        manager
            .alter_table(
                Table::alter()
                    .table(RpcAccountingV2::Table)
                    .add_column(
                        ColumnDef::new(RpcAccountingV2::SumCreditsUsed)
                            .big_unsigned()
                            .not_null()
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Replace the sample below with your own migration scripts
        manager
            .alter_table(
                sea_query::Table::alter()
                    .table(RpcAccountingV2::Table)
                    .drop_column(RpcAccountingV2::SumCreditsUsed)
                    .to_owned()
            )
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum RpcAccountingV2 {
    Table,
    Id,
    RpcKeyId,
    ChainId,
    Origin,
    PeriodDatetime,
    Method,
    ArchiveNeeded,
    ErrorResponse,
    FrontendRequests,
    BackendRequests,
    BackendRetries,
    NoServers,
    CacheMisses,
    CacheHits,
    SumRequestBytes,
    SumResponseMillis,
    SumResponseBytes,
    SumCreditsUsed
}
