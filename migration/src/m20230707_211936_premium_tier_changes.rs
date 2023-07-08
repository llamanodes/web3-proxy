use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Replace the sample below with your own migration scripts
        let update_out_of_funds = Query::update()
            .table(UserTier::Table)
            .limit(1)
            .values([
                (UserTier::MaxRequestsPerPeriod, Some("3900").into()),
                (UserTier::MaxConcurrentRequests, Some("3").into()),
            ])
            .and_where(Expr::col(UserTier::Title).eq("Premium Out Of Funds"))
            .to_owned();

        manager.exec_stmt(update_out_of_funds).await?;

        let update_premium = Query::update()
            .table(UserTier::Table)
            .limit(1)
            .values([
                (UserTier::MaxRequestsPerPeriod, None::<&str>.into()),
                (UserTier::MaxConcurrentRequests, Some("20").into()),
            ])
            .and_where(Expr::col(UserTier::Title).eq("Premium"))
            .to_owned();

        manager.exec_stmt(update_premium).await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let update_out_of_funds = Query::update()
            .table(UserTier::Table)
            .limit(1)
            .values([
                (UserTier::MaxRequestsPerPeriod, Some("6000").into()),
                (UserTier::MaxConcurrentRequests, Some("5").into()),
            ])
            .and_where(Expr::col(UserTier::Title).eq("Premium Out Of Funds"))
            .to_owned();

        manager.exec_stmt(update_out_of_funds).await?;

        let update_premium = Query::update()
            .table(UserTier::Table)
            .limit(1)
            .values([
                (UserTier::MaxRequestsPerPeriod, None::<&str>.into()),
                (UserTier::MaxConcurrentRequests, Some("100").into()),
            ])
            .and_where(Expr::col(UserTier::Title).eq("Premium"))
            .to_owned();

        manager.exec_stmt(update_premium).await?;

        Ok(())
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum UserTier {
    Table,
    Title,
    MaxRequestsPerPeriod,
    MaxConcurrentRequests,
}
