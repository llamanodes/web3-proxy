use anyhow::Context;
use argh::FromArgs;
use entities::{rpc_key, user, user_tier};
use log::{debug, info};
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, IntoActiveModel,
    QueryFilter,
};
use uuid::Uuid;
use web3_proxy::frontend::authorization::RpcSecretKey;

/// change a user's tier.
#[derive(FromArgs, PartialEq, Eq, Debug)]
#[argh(subcommand, name = "change_user_tier")]
pub struct ChangeUserTierCommand {
    /// the title of the user tier you are going to modify.
    #[argh(positional)]
    user_tier_title: String,

    /// the amount of requests to allow per rate limit period
    #[argh(option)]
    max_requests_per_period: Option<u64>,

    /// the amount of concurret requests to allow from a single user
    #[argh(option)]
    max_concurrent_requests: Option<u32>,
}

impl ChangeUserTierCommand {
    // TODO: don't expose the RpcSecretKeys at all. Better to take a user/key id. this is definitely most convenient

    pub async fn main(self, db_conn: &DatabaseConnection) -> anyhow::Result<()> {
        let user_tier = user_tier::Entity::find()
            .filter(user_tier::Column::Title.eq(self.user_tier_title))
            .one(db_conn)
            .await?
            .context("No user tier found with that name")?;

        debug!("initial user_tier: {:#?}", user_tier);

        let mut user_tier = user_tier.into_active_model();

        if let Some(max_requests_per_period) = self.max_requests_per_period {
            if user_tier.max_requests_per_period == sea_orm::Set(Some(max_requests_per_period)) {
                info!("max_requests_per_period already has this value");
            } else {
                user_tier.max_requests_per_period = sea_orm::Set(Some(max_requests_per_period));

                info!("changed max_requests_per_period")
            }
        }

        if let Some(max_concurrent_requests) = self.max_concurrent_requests {
            if user_tier.max_concurrent_requests == sea_orm::Set(Some(max_concurrent_requests)) {
                info!("max_concurrent_requests already has this value");
            } else {
                user_tier.max_concurrent_requests = sea_orm::Set(Some(max_concurrent_requests));

                info!("changed max_concurrent_requests")
            }
        }

        let user_tier = user_tier.save(db_conn).await?;

        debug!("new user_tier: {:#?}", user_tier);

        Ok(())
    }
}
