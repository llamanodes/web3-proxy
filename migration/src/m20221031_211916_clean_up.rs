use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // rename tables from plural to singluar
        manager
            .rename_table(
                Table::rename()
                    .table(Alias::new("revert_logs"), Alias::new("revert_log"))
                    .to_owned(),
            )
            .await?;

        manager
            .rename_table(
                Table::rename()
                    .table(Alias::new("rpc_keys"), Alias::new("rpc_key"))
                    .to_owned(),
            )
            .await?;

        // on rpc_key table, rename rpc_key to secret_key
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("rpc_key"))
                    .rename_column(Alias::new("rpc_key"), Alias::new("secret_key"))
                    .to_owned(),
            )
            .await?;

        // on revert_log table, rename user_key_id to rpc_key_id
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("revert_log"))
                    .rename_column(Alias::new("user_key_id"), Alias::new("rpc_key_id"))
                    .to_owned(),
            )
            .await?;

        // on rpc_accounting table, rename user_key_id to rpc_key_id
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("rpc_accounting"))
                    .rename_column(Alias::new("user_key_id"), Alias::new("rpc_key_id"))
                    .to_owned(),
            )
            .await?;

        // on secondary_users table, remove "email" and "address" column
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("secondary_user"))
                    .drop_column(Alias::new("email"))
                    .drop_column(Alias::new("address"))
                    .to_owned(),
            )
            .await?;

        // on user_tier table, rename requests_per_minute to max_requests_per_period
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("user_tier"))
                    .rename_column(
                        Alias::new("requests_per_minute"),
                        Alias::new("max_requests_per_period"),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("rpc_key"))
                    .drop_column(Alias::new("log_revert_chance"))
                    .add_column(
                        ColumnDef::new(Alias::new("log_revert_chance"))
                            .double()
                            .not_null()
                            .default(0.0),
                    )
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Replace the sample below with your own migration scripts
        todo!();
    }
}
