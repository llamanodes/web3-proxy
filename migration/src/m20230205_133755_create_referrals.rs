use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Create one table for the referrer
        manager
            .create_table(
                Table::create()
                    .table(Referrer::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Referrer::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(Referrer::ReferralCode)
                            .string()
                            .unique_key()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(Referrer::UserId)
                            .big_unsigned()
                            .unique_key()
                            .not_null(),
                    )
                    .foreign_key(
                        sea_query::ForeignKey::create()
                            .from(Referrer::Table, Referrer::UserId)
                            .to(User::Table, User::Id),
                    )
                    .to_owned(),
            )
            .await?;

        // Create one table for the referrer
        manager
            .create_table(
                Table::create()
                    .table(Referee::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Referee::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(Referee::CreditsAppliedForReferee)
                            .boolean()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(Referee::CreditsAppliedForReferrer)
                            .decimal_len(20, 10)
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(Referee::ReferralStartDate)
                            .date_time()
                            .not_null()
                            .extra("DEFAULT CURRENT_TIMESTAMP".to_string()),
                    )
                    .col(
                        ColumnDef::new(Referee::UsedReferralCode)
                            .integer()
                            .not_null(),
                    )
                    .foreign_key(
                        sea_query::ForeignKey::create()
                            .from(Referee::Table, Referee::UsedReferralCode)
                            .to(Referrer::Table, Referrer::Id),
                    )
                    .col(
                        ColumnDef::new(Referee::UserId)
                            .big_unsigned()
                            .unique_key()
                            .not_null(),
                    )
                    .foreign_key(
                        sea_query::ForeignKey::create()
                            .from(Referee::Table, Referee::UserId)
                            .to(User::Table, User::Id),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Referrer::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Referee::Table).to_owned())
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum Referrer {
    Table,
    Id,
    UserId,
    ReferralCode,
}

#[derive(Iden)]
enum Referee {
    Table,
    Id,
    UserId,
    UsedReferralCode,
    CreditsAppliedForReferrer,
    CreditsAppliedForReferee,
    ReferralStartDate,
}

#[derive(Iden)]
enum User {
    Table,
    Id,
}
