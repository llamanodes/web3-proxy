use anyhow::Context;
use argh::FromArgs;
use entities::{user, user_keys};
use ethers::prelude::Address;
use sea_orm::{ActiveModelTrait, TransactionTrait};
use tracing::info;
use uuid::Uuid;
use web3_proxy::users::new_api_key;

fn default_rpm() -> u32 {
    6_000_000
}

#[derive(FromArgs, PartialEq, Debug, Eq)]
/// Create a new user and api key
#[argh(subcommand, name = "create_user")]
pub struct CreateUserSubCommand {
    #[argh(option)]
    /// the user's ethereum address
    address: String,

    #[argh(option)]
    /// the user's optional email
    email: Option<String>,

    #[argh(option, default = "new_api_key()")]
    /// the user's first api key.
    /// If none given, one will be generated randomly.
    api_key: Uuid,

    #[argh(option, default = "default_rpm()")]
    /// maximum requests per minute
    rpm: u32,
}

impl CreateUserSubCommand {
    pub async fn main(self, db: &sea_orm::DatabaseConnection) -> anyhow::Result<()> {
        let txn = db.begin().await?;

        // TODO: would be nice to use the fixed array instead of a Vec in the entities
        let address = self
            .address
            .parse::<Address>()
            .context("Failed parsing new user address")?
            .to_fixed_bytes()
            .into();

        let u = user::ActiveModel {
            address: sea_orm::Set(address),
            email: sea_orm::Set(self.email),
            ..Default::default()
        };

        let u = u.save(&txn).await.context("Failed saving new user")?;

        info!(
            "user #{}: {:?}",
            u.id.as_ref(),
            Address::from_slice(u.address.as_ref())
        );

        // create a key for the new user
        // TODO: requests_per_minute should be configurable
        let uk = user_keys::ActiveModel {
            user_id: u.id,
            api_key: sea_orm::Set(self.api_key),
            requests_per_minute: sea_orm::Set(self.rpm),
            ..Default::default()
        };

        // TODO: if this fails, rever adding the user, too
        let uk = uk.save(&txn).await.context("Failed saving new user key")?;

        txn.commit().await?;

        info!("user key: {}", uk.api_key.as_ref());

        Ok(())
    }
}
