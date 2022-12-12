use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // allow null method
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("rpc_accounting"))
                    .modify_column(ColumnDef::new(Alias::new("method")).string().null())
                    .to_owned(),
            )
            .await?;

        // existing keys get set to detailed logging
        manager
            .alter_table(
                Table::alter()
                    .table(RpcKey::Table)
                    .add_column(
                        ColumnDef::new(RpcKey::LogLevel)
                            .enumeration(
                                Alias::new("log_level"),
                                [
                                    Alias::new("none"),
                                    Alias::new("aggregated"),
                                    Alias::new("detailed"),
                                ],
                            )
                            .not_null()
                            .default("detailed"),
                    )
                    .to_owned(),
            )
            .await?;

        // new keys get set to no logging
        manager
            .alter_table(
                Table::alter()
                    .table(RpcKey::Table)
                    .modify_column(
                        ColumnDef::new(RpcKey::LogLevel)
                            .enumeration(
                                Alias::new("log_level"),
                                [
                                    Alias::new("none"),
                                    Alias::new("aggregated"),
                                    Alias::new("detailed"),
                                ],
                            )
                            .not_null()
                            .default("none"),
                    )
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(RpcKey::Table)
                    .drop_column(RpcKey::LogLevel)
                    .to_owned(),
            )
            .await?;

        Ok(())
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum RpcKey {
    Table,
    LogLevel,
}
