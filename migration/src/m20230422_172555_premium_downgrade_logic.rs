use crate::sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Add a column "downgrade_tier_id"
        // It is a "foreign key" that references other items in this table
        manager
            .alter_table(
                Table::alter()
                    .table(UserTier::Table)
                    .add_column(ColumnDef::new(UserTier::DowngradeTierId).big_unsigned())
                    .add_foreign_key(
                        TableForeignKey::new()
                            .to_tbl(UserTier::Table)
                            .from_col(UserTier::DowngradeTierId)
                            .to_col(UserTier::Id),
                    )
                    .to_owned(),
            )
            .await?;

        // Insert Premium, and PremiumOutOfFunds
        let premium_out_of_funds_tier = Query::insert()
            .into_table(UserTier::Table)
            .columns([
                UserTier::Title,
                UserTier::MaxRequestsPerPeriod,
                UserTier::MaxConcurrentRequests,
                UserTier::DowngradeTierId,
            ])
            .values_panic([
                "Premium Out Of Funds".into(),
                Some("6000").into(),
                Some("5").into(),
                None::<i64>.into(),
            ])
            .to_owned();

        manager.exec_stmt(premium_out_of_funds_tier).await?;

        // Insert Premium Out Of Funds
        // get the premium tier ...
        let db_conn = manager.get_connection();
        let db_backend = manager.get_database_backend();

        let select_premium_out_of_funds_tier_id = Query::select()
            .column(UserTier::Id)
            .from(UserTier::Table)
            .cond_where(Expr::col(UserTier::Title).eq("Premium Out Of Funds"))
            .to_owned();
        let premium_out_of_funds_tier_id: u64 = db_conn
            .query_one(db_backend.build(&select_premium_out_of_funds_tier_id))
            .await?
            .expect("we just created Premium Out Of Funds")
            .try_get("", &UserTier::Id.to_string())?;

        // Add two tiers for premium: premium, and premium-out-of-funds
        let premium_tier = Query::insert()
            .into_table(UserTier::Table)
            .columns([
                UserTier::Title,
                UserTier::MaxRequestsPerPeriod,
                UserTier::MaxConcurrentRequests,
                UserTier::DowngradeTierId,
            ])
            .values_panic([
                "Premium".into(),
                None::<&str>.into(),
                Some("100").into(),
                Some(premium_out_of_funds_tier_id).into(),
            ])
            .to_owned();

        manager.exec_stmt(premium_tier).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Remove the two tiers that you just added
        // And remove the column you just added
        let db_conn = manager.get_connection();
        let db_backend = manager.get_database_backend();

        let delete_premium = Query::delete()
            .from_table(UserTier::Table)
            .cond_where(Expr::col(UserTier::Title).eq("Premium"))
            .to_owned();

        db_conn.execute(db_backend.build(&delete_premium)).await?;

        let delete_premium_out_of_funds = Query::delete()
            .from_table(UserTier::Table)
            .cond_where(Expr::col(UserTier::Title).eq("Premium Out Of Funds"))
            .to_owned();

        db_conn
            .execute(db_backend.build(&delete_premium_out_of_funds))
            .await?;

        // Finally drop the downgrade column
        manager
            .alter_table(
                Table::alter()
                    .table(UserTier::Table)
                    .drop_column(UserTier::DowngradeTierId)
                    .to_owned(),
            )
            .await
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
