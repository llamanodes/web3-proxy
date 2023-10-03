use tracing::{debug, info};
use web3_proxy::prelude::anyhow::{self, Context};
use web3_proxy::prelude::argh::{self, FromArgs};
use web3_proxy::prelude::entities::{rpc_key, user};
use web3_proxy::prelude::ethers::types::Address;
use web3_proxy::prelude::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, IntoActiveModel,
    QueryFilter,
};
use web3_proxy::prelude::uuid::Uuid;
use web3_proxy::secrets::RpcSecretKey;

/// change a key's owner.
#[derive(FromArgs, PartialEq, Eq, Debug)]
#[argh(subcommand, name = "transfer_key")]
pub struct TransferKeySubCommand {
    #[argh(positional)]
    /// the RPC key that you want to transfer.
    rpc_secret_key: RpcSecretKey,

    /// the new owner for the key.
    #[argh(positional)]
    new_address: String,
}

impl TransferKeySubCommand {
    pub async fn main(self, db_conn: &DatabaseConnection) -> anyhow::Result<()> {
        let rpc_secret_key: Uuid = self.rpc_secret_key.into();

        let new_address: Address = self.new_address.parse()?;

        let uk = rpc_key::Entity::find()
            .filter(rpc_key::Column::SecretKey.eq(rpc_secret_key))
            .one(db_conn)
            .await?
            .context("No key found")?;

        debug!("user key: {}", serde_json::to_string(&uk)?);

        let new_u = user::Entity::find()
            .filter(user::Column::Address.eq(new_address.as_bytes()))
            .one(db_conn)
            .await?
            .context("No user found with that key")?;

        debug!("new user: {}", serde_json::to_string(&new_u)?);

        if new_u.id == uk.user_id {
            info!("user already owns that key");
        } else {
            let mut uk = uk.into_active_model();

            uk.user_id = sea_orm::Set(new_u.id);

            uk.save(db_conn).await?;

            info!("changed the key's owner");
        }

        Ok(())
    }
}
