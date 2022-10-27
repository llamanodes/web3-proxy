use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_query::table::ColumnDef;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // users
        manager
            .create_table(
                Table::create()
                    .table(User::Table)
                    .col(
                        ColumnDef::new(User::Id)
                            .big_unsigned()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(User::Address)
                            .binary_len(20)
                            .not_null()
                            .unique_key(),
                    )
                    .col(ColumnDef::new(User::Description).string())
                    .col(ColumnDef::new(User::Email).string())
                    .to_owned(),
            )
            .await?;

        // secondary users
        manager
            .create_table(
                Table::create()
                    .table(SecondaryUser::Table)
                    .col(
                        ColumnDef::new(SecondaryUser::Id)
                            .big_unsigned()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(SecondaryUser::UserId)
                            .big_unsigned()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(SecondaryUser::Address)
                            .binary_len(20)
                            .not_null(),
                    )
                    .col(ColumnDef::new(SecondaryUser::Description).string())
                    .col(ColumnDef::new(SecondaryUser::Email).string())
                    .col(
                        ColumnDef::new(SecondaryUser::Role)
                            .enumeration(
                                Alias::new("role"),
                                [
                                    Alias::new("owner"),
                                    Alias::new("admin"),
                                    Alias::new("collaborator"),
                                ],
                            )
                            .not_null(),
                    )
                    .index(sea_query::Index::create().col(SecondaryUser::Address))
                    .foreign_key(
                        sea_query::ForeignKey::create()
                            .from(SecondaryUser::Table, SecondaryUser::UserId)
                            .to(User::Table, User::Id),
                    )
                    .to_owned(),
            )
            .await?;

        // api keys
        manager
            .create_table(
                Table::create()
                    .table(UserKeys::Table)
                    .col(
                        ColumnDef::new(UserKeys::Id)
                            .big_unsigned()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(UserKeys::UserId).big_unsigned().not_null())
                    .col(
                        ColumnDef::new(UserKeys::ApiKey)
                            .uuid()
                            .not_null()
                            .unique_key(),
                    )
                    .col(ColumnDef::new(UserKeys::Description).string())
                    .col(
                        ColumnDef::new(UserKeys::PrivateTxs)
                            .boolean()
                            .default(true)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(UserKeys::Active)
                            .boolean()
                            .default(true)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(UserKeys::RequestsPerMinute)
                            .unsigned()
                            .default(true)
                            .not_null(),
                    )
                    .index(sea_query::Index::create().col(UserKeys::Active))
                    .foreign_key(
                        sea_query::ForeignKey::create()
                            .from(UserKeys::Table, UserKeys::UserId)
                            .to(User::Table, User::Id),
                    )
                    .to_owned(),
            )
            .await?;

        // it worked!
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(User::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(SecondaryUser::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(UserKeys::Table).to_owned())
            .await?;

        Ok(())
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum User {
    Table,
    Id,
    Address,
    Description,
    Email,
}

/*
-- TODO: foreign keys
-- TODO: how should we store addresses?
-- TODO: creation time?
-- TODO: permissions. likely similar to infura
// TODO: creation time?
*/
#[derive(Iden)]
enum SecondaryUser {
    Table,
    Id,
    UserId,
    Address,
    Description,
    Email,
    Role,
}

/*
-- TODO: foreign keys
-- TODO: index on rpc_key
-- TODO: what size for rpc_key
-- TODO: track active with a timestamp?
-- TODO: creation time?
-- TODO: requests_per_day? requests_per_second?,
-- TODO: more security features. likely similar to infura
*/
#[derive(Iden)]
enum UserKeys {
    Table,
    Id,
    UserId,
    ApiKey,
    Description,
    PrivateTxs,
    Active,
    RequestsPerMinute,
}
