use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(AdminTrail::Table)
                    .modify_column(ColumnDef::new(AdminTrail::Endpoint).text().not_null())
                    .modify_column(ColumnDef::new(AdminTrail::Payload).text().not_null())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(AdminTrail::Table)
                    .modify_column(ColumnDef::new(AdminTrail::Endpoint).string().not_null())
                    .modify_column(ColumnDef::new(AdminTrail::Payload).string().not_null())
                    .to_owned(),
            )
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum AdminTrail {
    Table,
    Endpoint,
    Payload,
}
