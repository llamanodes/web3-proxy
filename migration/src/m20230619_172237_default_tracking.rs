use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // new keys get set to aggregated logging
        // TODO: rename "none" to "minimal"
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
                            .default("detailed"),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // new keys get set to none logging
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
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum RpcKey {
    Table,
    LogLevel,
}
