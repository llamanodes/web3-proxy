use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Referee::Table)
                    .drop_column(Referee::CreditsAppliedForReferee)
                    .add_column(
                        ColumnDef::new(Referee::OneTimeBonusAppliedForReferee)
                            .decimal_len(20, 10)
                            .default("0.0")
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Referee::Table)
                    .drop_column(Referee::OneTimeBonusAppliedForReferee)
                    .add_column(
                        ColumnDef::new(Referee::CreditsAppliedForReferee)
                            .boolean()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await
    }
}

/// Learn more at https://docs.rs/sea-query#iden
#[derive(Iden)]
enum Referee {
    Table,
    CreditsAppliedForReferee,
    OneTimeBonusAppliedForReferee,
}
