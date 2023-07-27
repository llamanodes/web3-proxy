// TODO: a lot of this is copy/paste of the admin frontend endpoint for granting credits.
// that's easier than refactoring right now.
// it could be cleaned up, but this is a script that runs once so isn't worth spending tons of time on.

use anyhow::Context;
use argh::FromArgs;
use entities::{admin_increase_balance_receipt, user, user_tier};
use futures::TryStreamExt;
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, IntoActiveModel,
    PaginatorTrait, QueryFilter, QueryOrder, TransactionTrait,
};
use rust_decimal::Decimal;
use tracing::info;

#[derive(FromArgs, PartialEq, Debug)]
/// Grant credits to all the users in a tier (and change their tier to premium).
#[argh(subcommand, name = "mass_grant_credits")]
pub struct MassGrantCredits {
    #[argh(positional)]
    /// the name of the user tier whose users will be upgraded to premium
    tier_to_upgrade: String,

    #[argh(positional)]
    /// how many credits to give.
    credits: Decimal,
}

impl MassGrantCredits {
    pub async fn main(self, db_conn: &DatabaseConnection) -> anyhow::Result<()> {
        let old_user_tier = user_tier::Entity::find()
            .filter(user_tier::Column::Title.like(&self.tier_to_upgrade))
            .one(db_conn)
            .await?
            .context("no user tier found with that name")?;

        let new_user_tier = user_tier::Entity::find()
            .filter(user_tier::Column::Title.like("Premium"))
            .one(db_conn)
            .await?
            .context("no Premium user tier found")?;

        let mut user_stream = user::Entity::find()
            .filter(user::Column::UserTierId.eq(old_user_tier.id))
            .order_by_asc(user::Column::Id)
            .paginate(db_conn, 50)
            .into_stream();

        while let Some(users_to_upgrade) = user_stream.try_next().await? {
            let txn = db_conn.begin().await?;

            info!("Upgrading {} users", users_to_upgrade.len());

            for user_to_upgrade in users_to_upgrade {
                if self.credits > 0.into() {
                    let increase_balance_receipt = admin_increase_balance_receipt::ActiveModel {
                        amount: sea_orm::Set(self.credits),
                        // TODO: allow customizing the admin id
                        admin_id: sea_orm::Set(1),
                        deposit_to_user_id: sea_orm::Set(user_to_upgrade.id),
                        note: sea_orm::Set("mass grant credits".into()),
                        ..Default::default()
                    };
                    increase_balance_receipt.save(&txn).await?;
                }

                let mut user_to_upgrade = user_to_upgrade.into_active_model();

                user_to_upgrade.user_tier_id = sea_orm::Set(new_user_tier.id);

                user_to_upgrade.save(&txn).await?;
            }

            txn.commit().await?;

            // we can't invalidate balance caches because they are in another process. they do have short ttls though
        }

        info!("success");

        Ok(())
    }
}
