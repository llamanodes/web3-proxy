use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                sea_query::Table::alter()
                    .table(RpcAccountingV2::Table)
                    .to_owned()
                    // allow rpc_key_id to be null. Needed for public rpc stat tracking
                    .modify_column(
                        ColumnDef::new(RpcAccountingV2::RpcKeyId)
                            .big_unsigned()
                            .null(),
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
                    .to_owned()
                    .modify_column(
                        ColumnDef::new(RpcAccountingV2::RpcKeyId)
                            .big_unsigned()
                            .not_null()
                            .default(0),
                    )
                    .to_owned(),
            )
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum RpcAccountingV2 {
    Table,
    RpcKeyId,
}
