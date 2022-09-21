use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // add some fields to the UserKeys table
        manager
            .alter_table(
                sea_query::Table::alter()
                    .table(UserKeys::Table)
                    .to_owned()
                    // change requests per minute to a big_unsigned
                    .modify_column(
                        ColumnDef::new(UserKeys::RequestsPerMinute)
                            .big_unsigned()
                            .not_null(),
                    )
                    // add a column for logging reverts in the RevertLogs table
                    .add_column(
                        ColumnDef::new(UserKeys::LogReverts)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .to_owned(),
            )
            .await?;

        // create a table for logging reverting eth_call and eth_estimateGas
        manager
            .create_table(
                Table::create()
                    .table(RevertLogs::Table)
                    .col(
                        ColumnDef::new(RevertLogs::Id)
                            .big_unsigned()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(RevertLogs::UserKeyId)
                            .big_unsigned()
                            .not_null(),
                    )
                    .col(ColumnDef::new(RevertLogs::Timestamp).timestamp().not_null())
                    .col(
                        ColumnDef::new(RevertLogs::Method)
                            .enumeration(
                                "method",
                                ["eth_call", "eth_estimateGas", "eth_sendRawTransaction"],
                            )
                            .not_null(),
                    )
                    .col(ColumnDef::new(RevertLogs::To).binary_len(20).not_null())
                    .col(ColumnDef::new(RevertLogs::CallData).text().not_null())
                    .index(sea_query::Index::create().col(RevertLogs::To))
                    .foreign_key(
                        sea_query::ForeignKey::create()
                            .from(RevertLogs::Table, RevertLogs::UserKeyId)
                            .to(UserKeys::Table, UserKeys::Id),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // drop the new table
        manager
            .drop_table(Table::drop().table(RevertLogs::Table).to_owned())
            .await?;

        // put the UserKeys back to how it was before our migrations
        manager
            .alter_table(
                sea_query::Table::alter()
                    .table(UserKeys::Table)
                    .to_owned()
                    .modify_column(
                        ColumnDef::new(UserKeys::RequestsPerMinute)
                            .unsigned()
                            .not_null(),
                    )
                    .drop_column(UserKeys::LogReverts)
                    .to_owned(),
            )
            .await
    }
}

// copied from create_table.rs, but added
#[derive(Iden)]
pub enum UserKeys {
    Table,
    Id,
    // UserId,
    // ApiKey,
    // Description,
    // PrivateTxs,
    // Active,
    RequestsPerMinute,
    LogReverts,
}

#[derive(Iden)]
enum RevertLogs {
    Table,
    Id,
    UserKeyId,
    Method,
    CallData,
    To,
    Timestamp,
}
