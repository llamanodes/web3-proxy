use anyhow::Context;
use argh::FromArgs;
use entities::user;
use ethers::types::Address;
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, IntoActiveModel,
    QueryFilter,
};
use serde_json::json;
use tracing::{debug, info};

/// change a user's address.
#[derive(FromArgs, PartialEq, Eq, Debug)]
#[argh(subcommand, name = "change_user_address")]
pub struct ChangeUserAddressSubCommand {
    /// the address of the user you want to change
    #[argh(positional)]
    old_address: String,

    /// the address of the user you want to change
    #[argh(positional)]
    new_address: String,
}

impl ChangeUserAddressSubCommand {
    pub async fn main(self, db_conn: &DatabaseConnection) -> anyhow::Result<()> {
        let old_address: Address = self.old_address.parse()?;
        let new_address: Address = self.new_address.parse()?;

        let u = user::Entity::find()
            .filter(user::Column::Address.eq(old_address.as_bytes()))
            .one(db_conn)
            .await?
            .context("No user found with that address")?;

        debug!("initial user: {:#}", json!(&u));

        if u.address == new_address.as_bytes() {
            info!("user already has this address");
        } else {
            let mut u = u.into_active_model();

            u.address = sea_orm::Set(new_address.as_bytes().to_vec());

            let u = u.save(db_conn).await?;

            info!("updated user: {:#?}", u);
        }

        Ok(())
    }
}
