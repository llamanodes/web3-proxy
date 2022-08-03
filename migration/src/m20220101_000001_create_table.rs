use sea_orm_migration::prelude::*;

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
                            .big_integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(User::Address)
                            .string_len(42)
                            .not_null()
                            .unique_key(),
                    )
                    .col(ColumnDef::new(User::Description).string().not_null())
                    .col(ColumnDef::new(User::Email).string().not_null())
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
                            .big_integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(SecondaryUser::UserId)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(SecondaryUser::Address)
                            .string_len(42)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(SecondaryUser::Description)
                            .string()
                            .not_null(),
                    )
                    .col(ColumnDef::new(SecondaryUser::Email).string().not_null())
                    .col(
                        ColumnDef::new(SecondaryUser::Role)
                            .enumeration("role", ["owner", "admin", "collaborator"])
                            .not_null(),
                    )
                    .index(
                        sea_query::Index::create()
                            .name("idx-secondary_user-address")
                            .col(SecondaryUser::Address),
                    )
                    .foreign_key(
                        sea_query::ForeignKey::create()
                            .from(SecondaryUser::Table, SecondaryUser::UserId)
                            .to(User::Table, User::Id),
                    )
                    .to_owned(),
            )
            .await?;

        // block list for the transaction firewall
        manager
            .create_table(
                Table::create()
                    .table(BlockList::Table)
                    .col(
                        ColumnDef::new(BlockList::Id)
                            .big_integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(BlockList::Address)
                            .string()
                            .not_null()
                            .unique_key(),
                    )
                    .col(ColumnDef::new(BlockList::Description).string().not_null())
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
                            .big_integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(UserKeys::UserId).big_integer().not_null())
                    .col(
                        ColumnDef::new(UserKeys::ApiKey)
                            .string_len(32)
                            .not_null()
                            .unique_key(),
                    )
                    .col(ColumnDef::new(UserKeys::Description).string().not_null())
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
                    .index(
                        sea_query::Index::create()
                            .name("idx-user_keys-active")
                            .col(UserKeys::Active),
                    )
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
            .drop_table(Table::drop().table(BlockList::Table).to_owned())
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

// TODO: creation time?
#[derive(Iden)]
enum BlockList {
    Table,
    Id,
    Address,
    Description,
}

/*
-- TODO: foreign keys
-- TODO: index on api_key
-- TODO: what size for api_key
-- TODO: track active with a timestamp?
-- TODO: creation time?
-- TODO: requests_per_second INT,
-- TODO: requests_per_day INT,
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
}
