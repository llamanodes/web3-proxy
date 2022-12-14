use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(PendingLogin::Table)
                    .col(
                        ColumnDef::new(PendingLogin::Id)
                            .big_unsigned()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(PendingLogin::Nonce)
                            .uuid()
                            .not_null()
                            .unique_key(),
                    )
                    .col(ColumnDef::new(PendingLogin::Message).text().not_null())
                    .col(
                        ColumnDef::new(PendingLogin::ExpiresAt)
                            .timestamp()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(Login::Table)
                    .col(
                        ColumnDef::new(Login::Id)
                            .big_unsigned()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(Login::BearerToken)
                            .uuid()
                            .not_null()
                            .unique_key(),
                    )
                    .col(ColumnDef::new(Login::UserId).big_unsigned().not_null())
                    .col(
                        ColumnDef::new(PendingLogin::ExpiresAt)
                            .timestamp()
                            .not_null(),
                    )
                    .foreign_key(
                        ForeignKeyCreateStatement::new()
                            .from_col(Login::UserId)
                            .to_tbl(User::Table)
                            .to_col(User::Id),
                    )
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(PendingLogin::Table).to_owned())
            .await?;

        manager
            .drop_table(Table::drop().table(Login::Table).to_owned())
            .await?;

        Ok(())
    }
}

/// Partial table definition
#[derive(Iden)]
pub enum User {
    Table,
    Id,
}

#[derive(Iden)]
enum PendingLogin {
    Table,
    Id,
    Nonce,
    Message,
    ExpiresAt,
}

#[derive(Iden)]
enum Login {
    Table,
    Id,
    BearerToken,
    UserId,
}
