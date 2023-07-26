use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let update_out_of_funds_tier = Query::update()
            .table(UserTier::Table)
            .values([
                (UserTier::MaxRequestsPerPeriod, Some("3000").into()),
                (UserTier::MaxConcurrentRequests, Some("3").into()),
            ])
            .and_where(Expr::col((UserTier::Title).eq("Premium Out Of Funds")))
            .to_owned();

        manager.exec_stmt(update_out_of_funds_tier).await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}

#[derive(Iden)]
enum UserTier {
    Table,
    Id,
    Title,
    MaxRequestsPerPeriod,
    MaxConcurrentRequests,
}
