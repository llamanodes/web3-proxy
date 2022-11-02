use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("rpc_accounting"))
                    .add_column(
                        ColumnDef::new(Alias::new("archive_request"))
                            .boolean()
                            .not_null(),
                    )
                    .drop_column(Alias::new("backend_retries"))
                    .drop_column(Alias::new("no_servers"))
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("rpc_accounting"))
                    .drop_column(Alias::new("archive_request"))
                    .to_owned(),
            )
            .await?;

        Ok(())
    }
}
