use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .rename_table(
                Table::rename()
                    .table(Alias::new("user_keys"), Alias::new("rpc_keys"))
                    .to_owned(),
            )
            .await?;

        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("rpc_keys"))
                    .rename_column(Alias::new("api_key"), Alias::new("rpc_key"))
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("rpc_keys"))
                    .rename_column(Alias::new("rpc_key"), Alias::new("api_key"))
                    .to_owned(),
            )
            .await?;

        manager
            .rename_table(
                Table::rename()
                    .table(Alias::new("rpc_keys"), Alias::new("user_keys"))
                    .to_owned(),
            )
            .await?;

        Ok(())
    }
}
