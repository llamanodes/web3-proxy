use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;
use sea_orm_migration::sea_query::table::ColumnDef;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // tracking request limits per key is going to get annoying.
        // so now, we make a "user_tier" table that tracks different tiers of users.
        manager
            .create_table(
                Table::create()
                    .table(UserTier::Table)
                    .col(
                        ColumnDef::new(UserTier::Id)
                            .big_unsigned()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(UserTier::Title).string().not_null())
                    .col(ColumnDef::new(UserTier::RequestsPerMinute).big_unsigned())
                    .col(ColumnDef::new(UserTier::MaxConcurrentRequests).unsigned())
                    .to_owned(),
            )
            .await?;

        // seed data
        let user_tiers = Query::insert()
            .into_table(UserTier::Table)
            .columns([
                UserTier::Title,
                UserTier::RequestsPerMinute,
                UserTier::MaxConcurrentRequests,
            ])
            // // anon users get very low limits. these belong in config though, not the database
            // .values_panic(["Anonymous".into(), Some("120").into(), Some("1").into()])
            // free users get higher but still low limits
            .values_panic(["Free".into(), Some("360").into(), Some("5").into()])
            // private demos get unlimited request/second
            .values_panic([
                "Private Demo".into(),
                None::<&str>.into(),
                Some("2000").into(),
            ])
            // we will definitely have more tiers between "free" and "effectively unlimited"
            // incredibly high limits
            .values_panic([
                "Effectively Unlimited".into(),
                Some("6000000").into(),
                Some("10000").into(),
            ])
            // no limits
            .values_panic(["Unlimited".into(), None::<&str>.into(), None::<&str>.into()])
            .to_owned();

        manager.exec_stmt(user_tiers).await?;

        let db_conn = manager.get_connection();
        let db_backend = manager.get_database_backend();

        let select_private_demo_id = Query::select()
            .column(UserTier::Id)
            .column(UserTier::Title)
            .from(UserTier::Table)
            .and_having(Expr::col(UserTier::Title).eq("Private Demo"))
            .to_owned();
        let private_demo_id: u64 = db_conn
            .query_one(db_backend.build(&select_private_demo_id))
            .await?
            .expect("we just created Private Demo")
            .try_get("", &UserTier::Id.to_string())?;

        // add a foreign key between tiers and users. default to "Private Demo"
        manager
            .alter_table(
                Table::alter()
                    .table(User::Table)
                    .add_column(
                        ColumnDef::new(User::UserTierId)
                            .big_unsigned()
                            .default(private_demo_id)
                            .not_null(),
                    )
                    .add_foreign_key(
                        TableForeignKey::new()
                            .from_col(User::UserTierId)
                            .to_tbl(UserTier::Table)
                            .to_col(UserTier::Id),
                    )
                    .to_owned(),
            )
            .await?;

        // change default to free tier
        let select_free_id = Query::select()
            .column(UserTier::Id)
            .column(UserTier::Title)
            .from(UserTier::Table)
            .and_having(Expr::col(UserTier::Title).eq("Free"))
            .to_owned();
        let free_id: u64 = db_conn
            .query_one(db_backend.build(&select_free_id))
            .await?
            .expect("we just created Free")
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

        // delete requests per minute and max concurrent requests now that we have user tiers
        manager
            .alter_table(
                Table::alter()
                    .table(RpcKeys::Table)
                    .drop_column(RpcKeys::RequestsPerMinute)
                    .drop_column(RpcKeys::MaxConcurrentRequests)
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // TODO: drop the index first

        manager
            .drop_table(Table::drop().table(UserTier::Table).to_owned())
            .await

        // TODO: undo more
    }
}

/// partial table
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
    RequestsPerMinute,
    MaxConcurrentRequests,
}

/// partial table
#[derive(Iden)]
enum RpcKeys {
    Table,
    RequestsPerMinute,
    MaxConcurrentRequests,
}
