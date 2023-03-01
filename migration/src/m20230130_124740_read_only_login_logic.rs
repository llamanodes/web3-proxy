use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Add a read-only column to the table
        manager
            .alter_table(
                Table::alter()
                    .table(Login::Table)
                    .add_column(ColumnDef::new(Login::ReadOnly).boolean().not_null())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Drop the column from the table ...
        manager
            .alter_table(
                Table::alter()
                    .table(Login::Table)
                    .drop_column(Login::ReadOnly)
                    .to_owned(),
            )
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum Login {
    Table,
    // Id,
    // BearerToken,
    ReadOnly,
    // UserId,
}
