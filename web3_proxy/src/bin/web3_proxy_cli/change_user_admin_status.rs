use anyhow::Context;
use argh::FromArgs;
use entities::{admin, login, user};
use ethers::types::{Address, Bytes};
use ethers::utils::keccak256;
use http::StatusCode;
use log::{debug, info};
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, ModelTrait, IntoActiveModel,
    QueryFilter,
};
use web3_proxy::frontend::errors::FrontendErrorResponse;

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
            .context(format!("No user with this id found {:?}", address))?;

        debug!("user: {:#?}", user);

        // Check if there is a record in the database
        match admin::Entity::find()
            .filter(admin::Column::UserId.eq(address))
            .one(db_conn)
            .await? {
            Some(old_admin) if !should_be_admin => {
                // User is already an admin, but shouldn't be
                old_admin.delete(db_conn).await?;
            }
            None if should_be_admin => {
                // User is not an admin yet, but should be
                let new_admin = admin::ActiveModel {
                    user_id: sea_orm::Set(user.id),
                    ..Default::default()
                };
                new_admin.insert(db_conn).await?;
            }
            _ => {
                // Do nothing in this case
                debug!("no change needed for: {:#?}", user);
                // Early return
                return Ok(());
            }
        }

        // Get the bearer tokens of this user and delete them ...
        let bearer_tokens = login::Entity::find()
            .filter(login::Column::UserId.eq(user.id))
            .all(db_conn)
            .await?;

        // // TODO: Remove from Redis
        // // Remove multiple items simultaneously, but this should be quick let's not prematurely optimize
        // let recent_user_id_key = format!("recent_users:id:{}", app.config.chain_id);
        // let salt = app
        //     .config
        //     .public_recent_ips_salt
        //     .as_ref()
        //     .expect("public_recent_ips_salt must exist in here");
        //
        // // TODO: Also clear redis ...
        // let salted_user_id = format!("{}:{}", salt, bearer_token.user_id);
        // let hashed_user_id = Bytes::from(keccak256(salted_user_id.as_bytes()));
        // redis_conn
        //     .zrem(&recent_user_id_key, hashed_user_id.to_string())
        //     .await?;

        // Remove any user logins from the database (incl. bearer tokens)
        let delete_result = login::Entity::delete_many()
            .filter(login::Column::UserId.eq(user.id))
            .exec(db_conn)
            .await?;

        debug!("cleared modified logins: {:?}", delete_result);

        Ok(())
    }
}
