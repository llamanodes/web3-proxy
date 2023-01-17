use anyhow::Context;
use argh::FromArgs;
use entities::{admin, user};
use ethers::types::Address;
use log::{debug, info};
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, ModelTrait, IntoActiveModel,
    QueryFilter,
};

/// change a user's admin status. eiter they are an admin, or they aren't
#[derive(FromArgs, PartialEq, Eq, Debug)]
#[argh(subcommand, name = "change_admin_status")]
pub struct ChangeUserAdminStatusSubCommand {
    /// the address of the user whose admin status you want to modify
    #[argh(positional)]
    address: String,

    /// true if the user should be an admin, false otherwise
    #[argh(positional)]
    should_be_admin: bool,
}

impl ChangeUserAdminStatusSubCommand {
    pub async fn main(self, db_conn: &DatabaseConnection) -> anyhow::Result<()> {
        let address: Address = self.address.parse()?;
        let should_be_admin: bool = self.should_be_admin;

        let address: Vec<u8> = address.to_fixed_bytes().into();

        // Find user in database
        let user = user::Entity::find()
            .filter(user::Column::Address.eq(address.clone()))
            .one(db_conn)
            .await?
            .context("No user found with that address")?;

        // Check if there is a record in the database
        let mut admin = admin::Entity::find()
            .filter(admin::Column::UserId.eq(address))
            .all(db_conn)
            .await?;

        debug!("user: {:#?}", user);

        match admin.pop() {
            None if should_be_admin => {
                // User is not an admin yet, but should be
                let new_admin = admin::ActiveModel {
                    user_id: sea_orm::Set(user.id),
                    ..Default::default()
                };
                new_admin.insert(db_conn).await?;
            },
            Some(old_admin) if !should_be_admin => {
                // User is already an admin, but shouldn't be
                old_admin.delete(db_conn).await?;
            },
            _ => {}
        }

        Ok(())
    }
}
