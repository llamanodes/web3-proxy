//! Increase requests per minute for the free tier to be better than our public tier (which has 3900/min)
use sea_orm_migration::{prelude::*, sea_orm::ConnectionTrait};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db_conn = manager.get_connection();
        let db_backend = manager.get_database_backend();

        let update_free = Query::update()
            .table(UserTier::Table)
            .value(UserTier::MaxRequestsPerPeriod, 6000)
            .and_where(Expr::col(UserTier::Title).eq("Free"))
            .limit(1)
            .to_owned();

        let x = db_backend.build(&update_free);

        let rows_affected = db_conn.execute(x).await?.rows_affected();

        assert_eq!(rows_affected, 1, "unable to update free tier");

        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        todo!();
    }
}

#[derive(Iden)]
enum UserTier {
    Table,
    Title,
    MaxRequestsPerPeriod,
}
