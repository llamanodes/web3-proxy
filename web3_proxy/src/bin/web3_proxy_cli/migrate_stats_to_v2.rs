use std::net::IpAddr;
use anyhow::Context;
use argh::FromArgs;
use entities::{rpc_key, rpc_accounting, rpc_accounting_v2, user};
use ethers::types::Address;
use hashbrown::HashMap;
use log::{debug, info, warn};
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, IntoActiveModel,
    QueryFilter, QuerySelect
};
use web3_proxy::app::AuthorizationChecks;
use web3_proxy::frontend::authorization::{Authorization, AuthorizationType, RequestMetadata};
use web3_proxy::stats::{BufferedRpcQueryStats, RpcQueryKey};

/// change a user's address.
#[derive(FromArgs, PartialEq, Eq, Debug)]
#[argh(subcommand, name = "migrate_stats_to_v2")]
pub struct MigrateStatsToV2 {}

impl MigrateStatsToV2 {
    pub async fn main(self, db_conn: &DatabaseConnection) -> anyhow::Result<()> {

        while true {

            // (1) Load a batch of rows out of the old table until no more rows are left
            let old_records = rpc_accounting::Entity::find()
                .filter(rpc_accounting::Column::Migrated.is_null())
                .limit(10000)
                .all(db_conn)
                .await?;
            if old_records.len() == 0 {
                // Break out of while loop once all records have successfully been migrated ...
                warn!("All records seem to have been successfully migrated!");
                break;
            }

            // (2) Create request metadata objects to match the old data
            let mut global_timeseries_buffer = HashMap::<RpcQueryKey, BufferedRpcQueryStats>::new();
            let mut opt_in_timeseries_buffer = HashMap::<RpcQueryKey, BufferedRpcQueryStats>::new();
            let mut accounting_db_buffer = HashMap::<RpcQueryKey, BufferedRpcQueryStats>::new();

            // Iterate through all old rows, and put them into the above objects.
            for x in old_records {

                info!("Preparing for migration: {:?}", x);

                // TODO: Split up a single request into multiple requests ...
                // according to frontend-requests, backend-requests, etc.

                // Get the rpc-key from the rpc_key_id
                // Get the user-id from the rpc_key_id
                let rpc_key_obj = rpc_key::Entity::find()
                    .filter(rpc_key::Column::id.eq(x.rpc_key_id))
                    .one(db_conn)
                    .await?;

                // TODO: Create authrization
                // We can probably also randomly generate this, as we don't care about the user (?)
                let authorization_checks = AuthorizationChecks {
                    user_id: rpc_key_id.user_id,
                    rpc_secret_key: rpc_key_id.secret_key,
                    rpc_secret_key_id: x.rpc_key_id,
                    ..Default::default()
                };
                // Then overwrite rpc_key_id and user_id (?)
                let authorization_type = AuthorizationType::Frontend;
                let authorization = Authorization::try_new(
                    authorization_checks,
                    None,
                    IpAddr::default(),
                    None,
                    None,
                    None,
                    authorization_type
                );

                // It will be like a fork basically (to simulate getting multiple single requests ...)
                // Iterate through all frontend requests
                // For each frontend request, create one object that will be emitted (make sure the timestamp is new)

                // TODO: Create RequestMetadata
                let request_metadata = RequestMetadata {
                    start_instant: x.period_datetime.timestamp(),
                    request_bytes: x.request_bytes,
                    archive_request: x.archive_request,
                    backend_requests: todo!(),
                    no_servers: 0,  // This is not relevant in the new version
                    error_response: x.error_response,
                    response_bytes: todo!(),
                    response_millis: todo!(),
                    response_from_backup_rpc: todo!() // I think we did not record this back then // Default::default()
                };

                // I suppose method cannot be empty ... (?)
                // Ok, now we basically create a state event ..
                RpcQueryStats::new(
                    x.method.unwrap(),
                    authorization
                    request_metadata,
                    x.response_bytes
                )

                // // For each of the old rows, create a (i) RpcQueryKey and a matching BufferedRpcQueryStats object
                // let key = RpcQueryKey {
                //     response_timestamp: x.period_datetime.timestamp(),
                //     archive_needed: x.archive_needed,
                //     error_response: x.error_response,
                //     period_datetime: x.period_datetime.timestamp(),
                //     rpc_secret_key_id: x.rpc_key_id,
                //     origin: x.origin,
                //     method: x.method
                // };
                //
                // // Create the corresponding BufferedRpcQueryStats object
                // let val = BufferedRpcQueryStats {
                //     frontend_requests: x.frontend_requests,
                //     backend_requests: x.backend_requests,
                //     backend_retries: x.backend_retries,
                //     no_servers: x.no_servers,
                //     cache_misses: x.cache_misses,
                //     cache_hits: x.cache_hits,
                //     sum_request_bytes: x.sum_request_bytes,
                //     sum_response_bytes: x.sum_response_bytes,
                //     sum_response_millis: x.sum_response_millis
                // };
                //
                // // TODO: Create authorization, request metadata, and bytes ... but bytes we don't really keep track of!
                // // We can generate dummy bytes of the same length though, this may work as well
                //
                // // TODO: Period datetime is also a question of what it is
                // // let response_stat = RpcQueryStats::new(
                // //     x.method,
                // //     authorization.clone(),
                // //     request_metadata.clone(),
                // //     response_bytes,
                // //     x.period_datetime
                // // );
                //
                // // BufferedRpcQueryStats
            }

            // (N-1) Mark the batch as migrated
            break;

        }





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
