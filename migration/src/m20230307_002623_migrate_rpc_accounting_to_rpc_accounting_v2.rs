use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Add a nullable timestamp column to check if things were migrated in the rpc_accounting table ..
        manager
            .alter_table(
                Table::alter()
                    .table(RpcAccounting::Table)
                    .to_owned()
                    .add_column(
                        ColumnDef::new(RpcAccounting::Migrated)
                                .timestamp()
                    )
                    .to_owned()
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(RpcAccounting::Table)
                    .drop_column(RpcAccounting::Migrated)
                    .to_owned()
            )
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
pub enum UserKeys {
    Table,
    Id,
}

#[derive(Iden)]
enum RpcAccounting {
    Table,
    Id,
    UserKeyId,
    ChainId,
    Method,
    ErrorResponse,
    PeriodDatetime,
    FrontendRequests,
    BackendRequests,
    BackendRetries,
    NoServers,
    CacheMisses,
    CacheHits,
    SumRequestBytes,
    MinRequestBytes,
    MeanRequestBytes,
    P50RequestBytes,
    P90RequestBytes,
    P99RequestBytes,
    MaxRequestBytes,
    SumResponseMillis,
    MinResponseMillis,
    MeanResponseMillis,
    P50ResponseMillis,
    P90ResponseMillis,
    P99ResponseMillis,
    MaxResponseMillis,
    SumResponseBytes,
    MinResponseBytes,
    MeanResponseBytes,
    P50ResponseBytes,
    P90ResponseBytes,
    P99ResponseBytes,
    MaxResponseBytes,
    Migrated
}

