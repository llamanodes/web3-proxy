// TODO: a lot of this is copy/paste of the admin frontend endpoint for granting credits.
// that's easier than refactoring right now.
// it could be cleaned up, but this is a script that runs once so isn't worth spending tons of time on.

use crate::balance::Balance;
use anyhow::Context;
use argh::FromArgs;
use entities::{admin_increase_balance_receipt, user, user_tier};
use ethers::types::Address;
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, IntoActiveModel,
    QueryFilter, TransactionTrait,
};
use rust_decimal::Decimal;
use serde_json::json;
use tracing::info;

#[derive(FromArgs, PartialEq, Debug)]
/// Grant credits to all the users in a tier (and change their tier to premium).
#[argh(subcommand, name = "grant_credits_to_address")]
pub struct GrantCreditsToAddress {
    #[argh(positional)]
    /// the address of the user tier whose users will be upgraded to premium
    user_address: Address,

    #[argh(positional)]
    /// how many credits to give. "0" to just see their balance
    credits: Decimal,
}

impl GrantCreditsToAddress {
    pub async fn main(self, db_conn: &DatabaseConnection) -> anyhow::Result<()> {
        let premium_user_tier = user_tier::Entity::find()
            .filter(user_tier::Column::Title.like("Premium"))
            .one(db_conn)
            .await?
            .context("no Premium user tier found")?;

        let user = user::Entity::find()
            .filter(user::Column::Address.eq(self.user_address.as_bytes()))
            .one(db_conn)
            .await?
            .context("no user")?;

        let user_id = user.id;

        let txn = db_conn.begin().await?;

        if self.credits > 0.into() {
            let increase_balance_receipt = admin_increase_balance_receipt::ActiveModel {
                amount: sea_orm::Set(self.credits),
                // TODO: allow customizing the admin id
                admin_id: sea_orm::Set(1),
                deposit_to_user_id: sea_orm::Set(user_id),
                note: sea_orm::Set("grant credits to address".into()),
                ..Default::default()
            };
            increase_balance_receipt.save(&txn).await?;
        }

        let mut user = user.into_active_model();

        user.user_tier_id = sea_orm::Set(premium_user_tier.id);

        if user.is_changed() {
            user.save(&txn).await?;
        }

        txn.commit().await?;

        let balance = Balance::try_from_db(db_conn, user_id)
            .await?
            .context("no balance")?;

        info!("{:?} balance: {:#}", self.user_address, json!(balance));

        Ok(())
    }
}
