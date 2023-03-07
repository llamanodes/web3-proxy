use anyhow::Context;
use argh::FromArgs;
use entities::user;
use ethers::types::Address;
use log::{debug, info};
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, IntoActiveModel,
    QueryFilter,
};

/// change a user's address.
#[derive(FromArgs, PartialEq, Eq, Debug)]
#[argh(subcommand, name = "migrate_stats_to_v2")]
pub struct MigrateStatsToV2 {}

impl MigrateStatsToV2 {
    pub async fn main(self, db_conn: &DatabaseConnection) -> anyhow::Result<()> {

        // (1) Load a batch of rows out of the old table

        // (2) Create request metadata objects to match the old data

        // (3) Update the batch in the old table with the current timestamp

        // (4) Send through a channel to a stat emitter



        // let old_address: Address = self.old_address.parse()?;
        // let new_address: Address = self.new_address.parse()?;
        //
        // let old_address: Vec<u8> = old_address.to_fixed_bytes().into();
        // let new_address: Vec<u8> = new_address.to_fixed_bytes().into();
        //
        // let u = user::Entity::find()
        //     .filter(user::Column::Address.eq(old_address))
        //     .one(db_conn)
        //     .await?
        //     .context("No user found with that address")?;
        //
        // debug!("initial user: {:#?}", u);
        //
        // if u.address == new_address {
        //     info!("user already has this address");
        // } else {
        //     let mut u = u.into_active_model();
        //
        //     u.address = sea_orm::Set(new_address);
        //
        //     let u = u.save(db_conn).await?;
        //
        //     info!("changed user address");
        //
        //     debug!("updated user: {:#?}", u);
        // }

        Ok(())
    }
}
