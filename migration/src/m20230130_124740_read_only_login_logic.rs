use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Replace the sample below with your own migration scripts
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("login"))
                    .add_column(
                        ColumnDef::new(Login::ReadOnly)
                            .boolean()
                            .not_null()
                    ).to_owned()
            ).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Replace the sample below with your own migration scripts
        // Drop the column from the table ...
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("login"))
                    .drop_column(Alias::new("read_only"))
                    .to_owned()
            ).await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum Login {
    Table,
    Id,
    BearerToken,
    ReadOnly,
    UserId,
}
