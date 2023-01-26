use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(RpcAccountingV2::Table)
                    .col(
                        ColumnDef::new(RpcAccountingV2::Id)
                            .big_unsigned()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(RpcAccountingV2::RpcKeyId)
                            .big_unsigned()
                            .null(),
                    )
                    .col(
                        ColumnDef::new(RpcAccountingV2::ChainId)
                            .big_unsigned()
                            .not_null(),
                    )
                    .col(ColumnDef::new(RpcAccountingV2::Origin).string().null())
                    .col(
                        ColumnDef::new(RpcAccountingV2::PeriodDatetime)
                            .timestamp()
                            .not_null(),
                    )
                    .col(ColumnDef::new(RpcAccountingV2::Method).string().null())
                    .col(
                        ColumnDef::new(RpcAccountingV2::ArchiveNeeded)
                            .boolean()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RpcAccountingV2::ErrorResponse)
                            .boolean()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RpcAccountingV2::FrontendRequests)
                            .big_unsigned()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RpcAccountingV2::BackendRequests)
                            .big_unsigned()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RpcAccountingV2::BackendRetries)
                            .big_unsigned()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RpcAccountingV2::NoServers)
                            .big_unsigned()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RpcAccountingV2::CacheMisses)
                            .big_unsigned()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RpcAccountingV2::CacheHits)
                            .big_unsigned()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RpcAccountingV2::SumRequestBytes)
                            .big_unsigned()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RpcAccountingV2::SumResponseMillis)
                            .big_unsigned()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RpcAccountingV2::SumResponseBytes)
                            .big_unsigned()
                            .not_null(),
                    )
                    .foreign_key(
                        sea_query::ForeignKey::create()
                            .from(RpcAccountingV2::Table, RpcAccountingV2::RpcKeyId)
                            .to(RpcKey::Table, RpcKey::Id),
                    )
                    .index(sea_query::Index::create().col(RpcAccountingV2::ChainId))
                    .index(sea_query::Index::create().col(RpcAccountingV2::Origin))
                    .index(sea_query::Index::create().col(RpcAccountingV2::PeriodDatetime))
                    .index(sea_query::Index::create().col(RpcAccountingV2::Method))
                    .index(sea_query::Index::create().col(RpcAccountingV2::ArchiveNeeded))
                    .index(sea_query::Index::create().col(RpcAccountingV2::ErrorResponse))
                    .index(
                        sea_query::Index::create()
                            .col(RpcAccountingV2::RpcKeyId)
                            .col(RpcAccountingV2::ChainId)
                            .col(RpcAccountingV2::Origin)
                            .col(RpcAccountingV2::PeriodDatetime)
                            .col(RpcAccountingV2::Method)
                            .col(RpcAccountingV2::ArchiveNeeded)
                            .col(RpcAccountingV2::ErrorResponse)
                            .unique(),
                    )
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(RpcAccountingV2::Table).to_owned())
            .await?;

        Ok(())
    }
}

/// Partial table definition
#[derive(Iden)]
pub enum RpcKey {
    Table,
    Id,
}

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
}
