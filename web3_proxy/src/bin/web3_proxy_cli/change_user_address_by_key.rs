use anyhow::Context;
use argh::FromArgs;
use entities::{rpc_key, user};
use ethers::types::Address;
use log::{debug, info};
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, IntoActiveModel,
    QueryFilter,
};
use uuid::Uuid;
use web3_proxy::frontend::authorization::RpcSecretKey;

/// change a user's tier.
#[derive(FromArgs, PartialEq, Eq, Debug)]
#[argh(subcommand, name = "change_user_address_by_key")]
pub struct ChangeUserAddressByKeyCommand {
    #[argh(positional)]
    /// the RPC key owned by the user you want to change.
    rpc_secret_key: RpcSecretKey,

    /// the new address for the user.
    #[argh(positional)]
    new_address: String,
}

impl ChangeUserAddressByKeyCommand {
    pub async fn main(self, db_conn: &DatabaseConnection) -> anyhow::Result<()> {
        let rpc_secret_key: Uuid = self.rpc_secret_key.into();

        let new_address: Address = self.new_address.parse()?;

        let new_address: Vec<u8> = new_address.to_fixed_bytes().into();

        let uk = rpc_key::Entity::find()
            .filter(rpc_key::Column::SecretKey.eq(rpc_secret_key))
            .one(db_conn)
            .await?
            .context("No key found")?;

        debug!("user key: {:#?}", uk);

        // use the rpc secret key to get the user
        // TODO: get this with a join on rpc_key
        let u = user::Entity::find_by_id(uk.user_id)
            .one(db_conn)
            .await?
            .context("No user found with that key")?;

        debug!("user: {:#?}", u);

        if u.address == new_address {
            info!("user already has that address");
        } else {
            let mut u = u.into_active_model();

            u.address = sea_orm::Set(new_address);

            let u = u.save(db_conn).await?;

            debug!("user: {:#?}", u);

            info!("user's address changed");
        }

        Ok(())
    }
}
