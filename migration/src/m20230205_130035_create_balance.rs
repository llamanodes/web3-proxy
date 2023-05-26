use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Balance::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Balance::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(Balance::AvailableBalance)
                            .decimal_len(20, 10)
                            .not_null()
                            .default(0.0),
                    )
                    .col(
                        ColumnDef::new(Balance::UsedBalance)
                            .decimal_len(20, 10)
                            .not_null()
                            .default(0.0),
                    )
                    .col(
                        ColumnDef::new(Balance::UserId)
                            .big_unsigned()
                            .unique_key()
                            .not_null(),
                    )
                    .foreign_key(
                        sea_query::ForeignKey::create()
                            .from(Balance::Table, Balance::UserId)
                            .to(User::Table, User::Id),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Balance::Table).to_owned())
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum User {
    Table,
    Id,
}

#[derive(Iden)]
enum Balance {
    Table,
    Id,
    UserId,
    AvailableBalance,
    UsedBalance,
}
