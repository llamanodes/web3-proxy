use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db_conn = manager.get_connection();
        let db_backend = manager.get_database_backend();

        let select_free_id = Query::select()
            .column(UserTier::Id)
            .column(UserTier::Title)
            .from(UserTier::Table)
            .and_having(Expr::col(UserTier::Title).eq("Free"))
            .to_owned();

        let free_id: u64 = db_conn
            .query_one(db_backend.build(&select_free_id))
            .await?
            .expect("Free tier should always exist")
            .try_get("", &UserTier::Id.to_string())?;

        let user_tiers = Query::insert()
            .into_table(UserTier::Table)
            .columns([
                UserTier::Title,
                UserTier::MaxRequestsPerPeriod,
                UserTier::MaxConcurrentRequests,
                UserTier::DowngradeTierId,
            ])
            .values_panic([
                "High Concurrency".into(),
                None::<u64>.into(),
                Some(300).into(),
                Some(free_id).into(),
            ])
            .to_owned();

        manager.exec_stmt(user_tiers).await?;

        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
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
    DowngradeTierId,
}
