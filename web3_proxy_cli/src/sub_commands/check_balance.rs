use web3_proxy::balance::Balance;
use web3_proxy::prelude::anyhow::{self, Context};
use web3_proxy::prelude::argh::{self, FromArgs};
use web3_proxy::prelude::entities::user;
use web3_proxy::prelude::ethers::types::Address;
use web3_proxy::prelude::migration::sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter,
};
use web3_proxy::prelude::serde_json::json;
use web3_proxy::prelude::tracing::debug;

/// change a user's tier.
#[derive(FromArgs, PartialEq, Eq, Debug)]
#[argh(subcommand, name = "check_balance")]
pub struct CheckBalanceSubCommand {
    #[argh(positional)]
    /// the address of the user you want to check.
    user_address: Address,
}

impl CheckBalanceSubCommand {
    pub async fn main(self, db_conn: &DatabaseConnection) -> anyhow::Result<()> {
        // use the address to get the user
        let user = user::Entity::find()
            .filter(user::Column::Address.eq(self.user_address.as_bytes()))
            .one(db_conn)
            .await?
            .context("No user found with that key")?;

        // TODO: don't serialize the rpc key
        debug!("user: {:#}", json!(&user));

        let balance = Balance::try_from_db(db_conn, user.id).await?;
        debug!("balance: {:#}", json!(&balance));

        Ok(())
    }
}
