use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(AdminTrail::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(AdminTrail::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(AdminTrail::Caller).big_unsigned().not_null(), // TODO: Add Foreign Key
                    )
                    .foreign_key(
                        sea_query::ForeignKey::create()
                            .from(AdminTrail::Table, AdminTrail::Caller)
                            .to(User::Table, User::Id),
                    )
                    .col(
                        ColumnDef::new(AdminTrail::ImitatingUser).big_unsigned(), // Can be null bcs maybe we're just logging in / using endpoints that don't imitate a user
                                                                                  // TODO: Add Foreign Key
                    )
                    .foreign_key(
                        sea_query::ForeignKey::create()
                            .from(AdminTrail::Table, AdminTrail::ImitatingUser)
                            .to(User::Table, User::Id),
                    )
                    .col(ColumnDef::new(AdminTrail::Endpoint).string().not_null())
                    .col(ColumnDef::new(AdminTrail::Payload).string().not_null())
                    .col(
                        ColumnDef::new(AdminTrail::Timestamp)
                            .timestamp()
                            .not_null()
                            .extra("DEFAULT CURRENT_TIMESTAMP".to_string()),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(AdminTrail::Table).to_owned())
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum AdminTrail {
    Table,
    Id,
    Caller,
    ImitatingUser,
    Endpoint,
    Payload,
    Timestamp,
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum User {
    Table,
    Id,
    // Address,
    // Description,
    // Email,
}
