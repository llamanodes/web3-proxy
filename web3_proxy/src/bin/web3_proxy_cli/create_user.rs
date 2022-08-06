use anyhow::Context;
use argh::FromArgs;
use entities::{user, user_keys};
use ethers::types::Address;
use fstrings::{format_args_f, println_f};
use sea_orm::ActiveModelTrait;
use web3_proxy::users::new_api_key;

#[derive(FromArgs, PartialEq, Debug)]
/// Create a new user and api key
#[argh(subcommand, name = "create_user")]
pub struct CreateUserSubCommand {
    #[argh(option)]
    /// the user's ethereum address
    address: String,

    #[argh(option)]
    /// the user's optional email
    email: Option<String>,
}

impl CreateUserSubCommand {
    pub async fn main(self, db: &sea_orm::DatabaseConnection) -> anyhow::Result<()> {
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

        let u = u.insert(db).await.context("Failed saving new user")?;

        println_f!("user: {u:?}");

        // create a key for the new user
        let uk = user_keys::ActiveModel {
            user_id: sea_orm::Set(u.id),
            api_key: sea_orm::Set(new_api_key()),
            ..Default::default()
        };

        let uk = uk.insert(db).await.context("Failed saving new user key")?;

        println_f!("user key: {uk:?}");

        Ok(())
    }
}
