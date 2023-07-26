use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    /// change default to premium tier
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db_conn = manager.get_connection();
        let db_backend = manager.get_database_backend();

        let select_premium_id = Query::select()
            .column(UserTier::Id)
            .column(UserTier::Title)
            .from(UserTier::Table)
            .and_having(Expr::col(UserTier::Title).eq("Premium"))
            .to_owned();

        let premium_id: u64 = db_conn
            .query_one(db_backend.build(&select_premium_id))
            .await?
            .expect("Premium tier should exist")
            .try_get("", &UserTier::Id.to_string())?;

        manager
            .alter_table(
                Table::alter()
                    .table(User::Table)
                    .modify_column(
                        ColumnDef::new(User::UserTierId)
                            .big_unsigned()
                            .default(premium_id)
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    /// change default to free tier
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
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
            .expect("Free tier should exist")
            .try_get("", &UserTier::Id.to_string())?;

        manager
            .alter_table(
                Table::alter()
                    .table(User::Table)
                    .modify_column(
                        ColumnDef::new(User::UserTierId)
                            .big_unsigned()
                            .default(free_id)
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;

        Ok(())
    }
}

#[derive(Iden)]
enum User {
    Table,
    UserTierId,
}

#[derive(Iden)]
enum UserTier {
    Table,
    Id,
    Title,
}
