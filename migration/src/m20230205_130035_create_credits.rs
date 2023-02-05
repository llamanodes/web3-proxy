use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Credits::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Credits::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(Credits::Credits)
                            .big_unsigned()
                            .not_null()
                            .default(0)
                    )
                    .col(
                        ColumnDef::new(Credits::UserId)
                            .big_unsigned()
                            .unique_key()
                            .not_null()
                    )
                    .foreign_key(
                        sea_query::ForeignKey::create()
                            .from(Credits::Table, Credits::UserId)
                            .to(User::Table, User::Id),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Credits::Table).to_owned())
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum Credits {
    Table,
    Id,
    UserId,
    Credits,
}

#[derive(Iden)]
enum User {
    Table,
    Id,
    Address,
    Description,
    Email,
}