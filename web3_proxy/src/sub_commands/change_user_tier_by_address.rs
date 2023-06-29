use anyhow::Context;
use argh::FromArgs;
use entities::{user, user_tier};
use ethers::types::Address;
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, IntoActiveModel,
    QueryFilter,
};
use serde_json::json;
use tracing::{debug, info};

/// change a user's tier.
#[derive(FromArgs, PartialEq, Eq, Debug)]
#[argh(subcommand, name = "change_user_tier_by_address")]
pub struct ChangeUserTierByAddressSubCommand {
    #[argh(positional)]
    /// the address of the user you want to change.
    user_address: Address,

    /// the title of the desired user tier.
    #[argh(positional)]
    user_tier_title: String,
}

impl ChangeUserTierByAddressSubCommand {
    pub async fn main(self, db_conn: &DatabaseConnection) -> anyhow::Result<()> {
        // use the address to get the user
        let user = user::Entity::find()
            .filter(user::Column::Address.eq(self.user_address.as_bytes()))
            .one(db_conn)
            .await?
            .context("No user found with that key")?;

        // TODO: don't serialize the rpc key
        debug!("user: {:#}", json!(&user));

        // use the title to get the user tier
        let user_tier = user_tier::Entity::find()
            .filter(user_tier::Column::Title.eq(self.user_tier_title))
            .one(db_conn)
            .await?
            .context("No user tier found with that name")?;

        debug!("user_tier: {:#}", json!(&user_tier));

        if user.user_tier_id == user_tier.id {
            info!("user already has that tier");
        } else {
            let mut user = user.into_active_model();

            user.user_tier_id = sea_orm::Set(user_tier.id);

            user.save(db_conn).await?;

            info!("user's tier changed");
        }

        Ok(())
    }
}
