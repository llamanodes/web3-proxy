use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Referral::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Referral::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(Referral::ReferralCode)
                            .string()
                            .unique_key()
                            .not_null()
                    )
                    .col(
                        ColumnDef::new(Referral::UsedReferralCode)
                            .string()
                    )
                    // Basically, this links to who invited the user ...
                    .foreign_key(
                        sea_query::ForeignKey::create()
                            .from(Referral::Table, Referral::UsedReferralCode)
                            .to(Referral::Table, Referral::ReferralCode),
                    )
                    .col(
                        ColumnDef::new(Referral::UserId)
                            .big_unsigned()
                            .unique_key()
                            .not_null()
                    )
                    .foreign_key(
                        sea_query::ForeignKey::create()
                            .from(Referral::Table, Referral::UserId)
                            .to(User::Table, User::Id),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Referral::Table).to_owned())
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum Referral {
    Table,
    Id,
    UserId,
    ReferralCode,
    UsedReferralCode
}

#[derive(Iden)]
enum User {
    Table,
    Id,
    Address,
    Description,
    Email,
}
