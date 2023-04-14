use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(SecondaryUser::Table)
                    .add_column(
                        ColumnDef::new(SecondaryUser::RpcKey)
                            .big_unsigned()
                            .not_null(), // add foreign key to user table ...,
                    )
                    .add_foreign_key(
                        TableForeignKey::new()
                            .name("FK_secondary_user-rpc_key")
                            .from_tbl(SecondaryUser::Table)
                            .from_col(SecondaryUser::RpcKey)
                            .to_tbl(RpcKey::Table)
                            .to_col(RpcKey::Id)
                            .on_delete(ForeignKeyAction::NoAction)
                            .on_update(ForeignKeyAction::NoAction),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                sea_query::Table::alter()
                    .table(SecondaryUser::Table)
                    .drop_column(SecondaryUser::RpcKey)
                    .to_owned(),
            )
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum SecondaryUser {
    Table,
    RpcKey,
}

#[derive(Iden)]
enum RpcKey {
    Table,
    Id,
}
