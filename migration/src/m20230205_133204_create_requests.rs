use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(RpcRequest::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(RpcRequest::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(RpcRequest::TxHash)
                            .string()
                    )
                    // TODO: Should eventually be an enum ...
                    .col(
                        ColumnDef::new(RpcRequest::Chain)
                            .string()
                            .not_null()
                    )
                    .col(
                        ColumnDef::new(RpcRequest::UserId)
                            .big_unsigned()
                            .not_null()
                    )
                    .foreign_key(
                        sea_query::ForeignKey::create()
                            .from(RpcRequest::Table, RpcRequest::UserId)
                            .to(User::Table, User::Id),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(RpcRequest::Table).to_owned())
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum RpcRequest {
    Table,
    Id,
    UserId,
    TxHash,
    Chain
}

#[derive(Iden)]
enum User {
    Table,
    Id,
    Address,
    Description,
    Email,
}