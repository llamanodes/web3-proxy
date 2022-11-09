use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(RpcAccounting::Table)
                    .modify_column(
                        ColumnDef::new(RpcAccounting::RpcKeyId)
                            .big_unsigned()
                            .null(),
                    )
                    .add_column(ColumnDef::new(RpcAccounting::Origin).string().null())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(RpcAccounting::Table)
                    .modify_column(
                        ColumnDef::new(RpcAccounting::RpcKeyId)
                            .big_unsigned()
                            .not_null(),
                    )
                    .drop_column(RpcAccounting::Origin)
                    .to_owned(),
            )
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum RpcAccounting {
    Table,
    RpcKeyId,
    Origin,
}
