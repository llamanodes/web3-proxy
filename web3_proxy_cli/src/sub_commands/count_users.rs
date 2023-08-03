use tracing::info;
use web3_proxy::prelude::anyhow;
use web3_proxy::prelude::argh::{self, FromArgs};
use web3_proxy::prelude::entities::user;
use web3_proxy::prelude::migration::sea_orm::{self, EntityTrait, PaginatorTrait};

#[derive(FromArgs, PartialEq, Debug, Eq)]
/// Create a new user and api key
#[argh(subcommand, name = "count_users")]
pub struct CountUsersSubCommand {}

impl CountUsersSubCommand {
    pub async fn main(self, db: &sea_orm::DatabaseConnection) -> anyhow::Result<()> {
        let count = user::Entity::find().count(db).await?;

        info!("user count: {}", count);

        Ok(())
    }
}
