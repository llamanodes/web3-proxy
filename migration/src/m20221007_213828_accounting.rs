use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // create a table for rpc request accounting
        manager
            .create_table(
                Table::create()
                    .table(RpcAccounting::Table)
                    .col(
                        ColumnDef::new(RpcAccounting::Id)
                            .big_unsigned()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(RpcAccounting::UserKeyId)
                            .big_unsigned()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RpcAccounting::ChainId)
                            .big_unsigned()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RpcAccounting::Timestamp)
                            .timestamp()
                            .not_null(),
                    )
                    .col(ColumnDef::new(RpcAccounting::Method).string().not_null())
                    .col(
                        ColumnDef::new(RpcAccounting::FrontendRequests)
                            .unsigned()
                            .not_null(),
                    )
                    .col(
                        // 0 means cache hit
                        // 1 is hopefully what most require
                        // but there might be more if retries were necessary
                        ColumnDef::new(RpcAccounting::BackendRequests)
                            .unsigned()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RpcAccounting::ErrorResponse)
                            .boolean()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RpcAccounting::QueryMillis)
                            .unsigned()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RpcAccounting::RequestBytes)
                            .unsigned()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(RpcAccounting::ResponseBytes)
                            .unsigned()
                            .not_null(),
                    )
                    .index(sea_query::Index::create().col(RpcAccounting::Timestamp))
                    .index(sea_query::Index::create().col(RpcAccounting::Method))
                    .index(sea_query::Index::create().col(RpcAccounting::BackendRequests))
                    .foreign_key(
                        sea_query::ForeignKey::create()
                            .from(RpcAccounting::Table, RpcAccounting::UserKeyId)
                            .to(UserKeys::Table, UserKeys::Id),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(RpcAccounting::Table).to_owned())
            .await
    }
}

/// Partial table definition
#[derive(Iden)]
pub enum UserKeys {
    Table,
    Id,
}

#[derive(Iden)]
enum RpcAccounting {
    Table,
    Id,
    Timestamp,
    UserKeyId,
    ChainId,
    Method,
    FrontendRequests,
    BackendRequests,
    ErrorResponse,
    QueryMillis,
    RequestBytes,
    ResponseBytes,
}
