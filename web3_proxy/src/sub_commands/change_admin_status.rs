use anyhow::Context;
use argh::FromArgs;
use entities::{admin, login, user};
use ethers::types::Address;
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, ModelTrait, QueryFilter,
};
use serde_json::json;
use tracing::{debug, info};

/// change a user's admin status. eiter they are an admin, or they aren't
#[derive(FromArgs, PartialEq, Eq, Debug)]
#[argh(subcommand, name = "change_admin_status")]
pub struct ChangeAdminStatusSubCommand {
    /// the address of the user whose admin status you want to modify
    #[argh(positional)]
    pub address: String,

    /// true if the user should be an admin, false otherwise
    #[argh(positional)]
    pub should_be_admin: bool,
}

impl ChangeAdminStatusSubCommand {
    pub async fn main(self, db_conn: &DatabaseConnection) -> anyhow::Result<()> {
        let address: Address = self.address.parse()?;
        let should_be_admin: bool = self.should_be_admin;

        // Find user in database
        let user = user::Entity::find()
            .filter(user::Column::Address.eq(address.as_bytes()))
            .one(db_conn)
            .await?
            .context(format!("No user with this address found {:?}", address))?;

        debug!("user: {:#}", json!(&user));

        // Check if there is a record in the database
        match admin::Entity::find()
            .filter(admin::Column::UserId.eq(user.id))
            .one(db_conn)
            .await?
        {
            Some(old_admin) if !should_be_admin => {
                // User is already an admin, but shouldn't be
                old_admin.delete(db_conn).await?;
                info!("revoked admin status");
            }
            None if should_be_admin => {
                // User is not an admin yet, but should be
                let new_admin = admin::ActiveModel {
                    user_id: sea_orm::Set(user.id),
                    ..Default::default()
                };
                new_admin.insert(db_conn).await?;
                info!("granted admin status");
            }
            _ => {
                info!("no change needed for: {:#}", json!(user));
                // Since no change happened, we do not want to delete active logins. Return now.
                return Ok(());
            }
        }

        // Remove any user logins from the database (incl. bearer tokens)
        let delete_result = login::Entity::delete_many()
            .filter(login::Column::UserId.eq(user.id))
            .exec(db_conn)
            .await?;

        debug!("cleared modified logins: {:?}", delete_result);

        Ok(())
    }
}
