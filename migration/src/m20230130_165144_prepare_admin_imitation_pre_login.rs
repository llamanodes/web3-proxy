use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(PendingLogin::Table)
                    .add_column(ColumnDef::new(PendingLogin::ImitatingUser).big_unsigned())
                    .add_foreign_key(
                        TableForeignKey::new()
                            .name("fk-pending_login-imitating_user")
                            .from_tbl(PendingLogin::Table)
                            .to_tbl(User::Table)
                            .from_col(PendingLogin::ImitatingUser)
                            .to_col(User::Id),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(PendingLogin::Table)
                    .drop_foreign_key(Alias::new("fk-pending_login-imitating_user"))
                    .drop_column(PendingLogin::ImitatingUser)
                    .to_owned(),
            )
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum PendingLogin {
    Table,
    // Id,
    // Nonce,
    // Message,
    // ExpiresAt,
    ImitatingUser,
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum User {
    Table,
    Id,
}
