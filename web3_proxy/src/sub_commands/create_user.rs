use crate::frontend::authorization::RpcSecretKey;
use anyhow::Context;
use argh::FromArgs;
use entities::{rpc_key, user};
use ethers::prelude::Address;
use migration::sea_orm::{self, ActiveModelTrait, TransactionTrait};
use tracing::info;
use ulid::Ulid;
use uuid::Uuid;

#[derive(FromArgs, PartialEq, Debug, Eq)]
/// Create a new user and api key
#[argh(subcommand, name = "create_user")]
pub struct CreateUserSubCommand {
    /// the user's ethereum address or descriptive string.
    /// If a string is given, it will be converted to hex and potentially truncated.
    /// Users from strings are only for testing since they won't be able to log in.
    #[argh(option)]
    address: String,

    /// the user's optional email.
    #[argh(option)]
    email: Option<String>,

    /// the user's first api ULID or UUID key.
    /// If none given, one will be created.
    #[argh(option)]
    rpc_secret_key: Option<RpcSecretKey>,

    /// an optional short description of the key's purpose.
    #[argh(option)]
    description: Option<String>,
}

impl CreateUserSubCommand {
    pub async fn main(self, db: &sea_orm::DatabaseConnection) -> anyhow::Result<()> {
        let txn = db.begin().await?;

        // TODO: would be nice to use the fixed array instead of a Vec in the entities
        // take a simple String. If it starts with 0x, parse as address. otherwise convert ascii to hex
        let address = if self.address.starts_with("0x") {
            let address = self.address.parse::<Address>()?;

            address.to_fixed_bytes().into()
        } else {
            // left pad and truncate the string
            let address = &format!("{:\x00>20}", self.address)[0..20];

            // convert the string to bytes
            let bytes = address.as_bytes();

            // convert the slice to a Vec
            bytes.try_into().expect("Bytes can always be a Vec<u8>")
        };

        // TODO: get existing or create a new one
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

        let rpc_secret_key = self.rpc_secret_key.unwrap_or_else(RpcSecretKey::new);

        // create a key for the new user
        let uk = rpc_key::ActiveModel {
            user_id: u.id,
            secret_key: sea_orm::Set(rpc_secret_key.into()),
            description: sea_orm::Set(self.description),
            ..Default::default()
        };

        // TODO: if this fails, rever adding the user, too
        let _uk = uk.save(&txn).await.context("Failed saving new user key")?;

        txn.commit().await?;

        info!("user key as ULID: {}", Ulid::from(rpc_secret_key));
        info!("user key as UUID: {}", Uuid::from(rpc_secret_key));

        Ok(())
    }
}
