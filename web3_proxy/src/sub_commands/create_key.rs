use crate::frontend::authorization::RpcSecretKey;
use anyhow::Context;
use argh::FromArgs;
use entities::{rpc_key, user};
use ethers::prelude::Address;
use migration::sea_orm::{self, ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter};
use tracing::info;
use ulid::Ulid;
use uuid::Uuid;

#[derive(FromArgs, PartialEq, Debug, Eq)]
/// Create a new user and api key
#[argh(subcommand, name = "create_key")]
pub struct CreateKeySubCommand {
    /// the user's ethereum address or descriptive string.
    /// If a string is given, it will be converted to hex and potentially truncated.
    /// Users from strings are only for testing since they won't be able to log in.
    #[argh(positional)]
    address: Address,

    /// the user's api ULID or UUID key.
    /// If none given, one will be created.
    #[argh(option)]
    rpc_secret_key: Option<RpcSecretKey>,

    /// an optional short description of the key's purpose.
    #[argh(option)]
    description: Option<String>,
}

impl CreateKeySubCommand {
    pub async fn main(self, db: &sea_orm::DatabaseConnection) -> anyhow::Result<()> {
        // TODO: get existing or create a new one
        let u = user::Entity::find()
            .filter(user::Column::Address.eq(self.address.as_bytes()))
            .one(db)
            .await?
            .context("No user found with that address")?;

        info!("user #{}", u.id);

        let rpc_secret_key = self.rpc_secret_key.unwrap_or_else(RpcSecretKey::new);

        // create a key for the new user
        let uk = rpc_key::ActiveModel {
            user_id: sea_orm::Set(u.id),
            secret_key: sea_orm::Set(rpc_secret_key.into()),
            description: sea_orm::Set(self.description),
            ..Default::default()
        };

        let _uk = uk.save(db).await.context("Failed saving new user key")?;

        info!("user key as ULID: {}", Ulid::from(rpc_secret_key));
        info!("user key as UUID: {}", Uuid::from(rpc_secret_key));

        Ok(())
    }
}
