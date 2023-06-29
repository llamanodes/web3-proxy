use crate::frontend::authorization::RpcSecretKey;
use anyhow::Context;
use argh::FromArgs;
use entities::{rpc_key, user, user_tier};
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, IntoActiveModel,
    QueryFilter,
};
use serde_json::json;
use tracing::{debug, info};
use uuid::Uuid;

/// change a user's tier.
#[derive(FromArgs, PartialEq, Eq, Debug)]
#[argh(subcommand, name = "change_user_tier_by_key")]
pub struct ChangeUserTierByKeySubCommand {
    #[argh(positional)]
    /// the RPC key owned by the user you want to change.
    rpc_secret_key: RpcSecretKey,

    /// the title of the desired user tier.
    #[argh(positional)]
    user_tier_title: String,
}

impl ChangeUserTierByKeySubCommand {
    // TODO: don't expose the RpcSecretKeys at all. Better to take a user/key id. this is definitely most convenient

    pub async fn main(self, db_conn: &DatabaseConnection) -> anyhow::Result<()> {
        let rpc_secret_key: Uuid = self.rpc_secret_key.into();

        let user_tier = user_tier::Entity::find()
            .filter(user_tier::Column::Title.eq(self.user_tier_title))
            .one(db_conn)
            .await?
            .context("No user tier found with that name")?;

        debug!("user_tier: {:#}", json!(&user_tier));

        // use the rpc secret key to get the user
        let user = user::Entity::find()
            .inner_join(rpc_key::Entity)
            .filter(rpc_key::Column::SecretKey.eq(rpc_secret_key))
            .one(db_conn)
            .await?
            .context("No user found with that key")?;

        debug!("user: {:#}", json!(&user));

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
