use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // add a field to the UserKeys table
        manager
            .alter_table(
                sea_query::Table::alter()
                    .table(UserKeys::Table)
                    // add column for a better version of rate limiting
                    .add_column(
                        ColumnDef::new(UserKeys::MaxConcurrentRequests)
                            .big_unsigned()
                            .null()
                            .default(200),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // put the UserKeys back to how it was before our migrations
        manager
            .alter_table(
                sea_query::Table::alter()
                    .table(UserKeys::Table)
                    .to_owned()
                    .drop_column(UserKeys::MaxConcurrentRequests)
                    .to_owned(),
            )
            .await
    }
}

// copied from *_log_reverts.rs, but added new columns
#[derive(Iden)]
pub enum UserKeys {
    Table,
    // we don't touch some of the columns
    // Id,
    // UserId,
    // ApiKey,
    // Description,
    // PrivateTxs,
    // Active,
    // RequestsPerMinute,
    // LogRevertChance,
    // AllowedIps,
    // AllowedOrigins,
    // AllowedReferers,
    // AllowedUserAgents,
    MaxConcurrentRequests,
}
