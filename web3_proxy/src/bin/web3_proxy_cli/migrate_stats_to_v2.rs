use anyhow::Context;
use argh::FromArgs;
use chrono::{DateTime, Utc};
use entities::{rpc_accounting, rpc_accounting_v2, rpc_key, user};
use ethers::types::Address;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use hashbrown::HashMap;
use log::{debug, error, info, trace, warn};
use migration::sea_orm::QueryOrder;
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, IntoActiveModel,
    QueryFilter, QuerySelect, UpdateResult,
};
use migration::{Expr, Value};
use std::mem::swap;
use std::net::{IpAddr, Ipv4Addr};
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use tokio::time::{sleep, Instant};
use web3_proxy::app::{AuthorizationChecks, Web3ProxyApp, BILLING_PERIOD_SECONDS};
use web3_proxy::config::TopConfig;
use web3_proxy::frontend::authorization::{
    Authorization, AuthorizationType, RequestMetadata, RpcSecretKey,
};
use web3_proxy::stats::{BufferedRpcQueryStats, RpcQueryKey, RpcQueryStats, StatBuffer};

// Helper function to go from DateTime to Instant
fn datetime_utc_to_instant(datetime: DateTime<Utc>) -> anyhow::Result<Instant> {
    let epoch = datetime.timestamp(); // Get the Unix timestamp
    let nanos = datetime.timestamp_subsec_nanos();

    let duration_since_epoch = Duration::new(epoch as u64, nanos);
    // let duration_since_datetime = Duration::new(, nanos);
    let instant_new = Instant::now();
    warn!("Instant new is: {:?}", instant_new);
    let unix_epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    warn!("Instant since unix epoch is: {:?}", unix_epoch);

    instant_new
        .checked_sub(unix_epoch)
        .context("Could not subtract unix epoch from instant now")?
        .checked_add(duration_since_epoch)
        .context("Could not add duration since epoch for updated time")
}

#[derive(FromArgs, PartialEq, Eq, Debug)]
/// Migrate towards influxdb and rpc_accounting_v2 from rpc_accounting
#[argh(subcommand, name = "migrate_stats_to_v2")]
pub struct MigrateStatsToV2 {}

impl MigrateStatsToV2 {
    pub async fn main(
        self,
        top_config: TopConfig,
        db_conn: &DatabaseConnection,
    ) -> anyhow::Result<()> {
        let number_of_rows_to_process_at_once = 2000;

        // we wouldn't really need this, but let's spawn this anyways
        // easier than debugging the rest I suppose
        let (app_shutdown_sender, _app_shutdown_receiver) = broadcast::channel(1);
        let rpc_account_shutdown_recevier = app_shutdown_sender.subscribe();

        // we must wait for these to end on their own (and they need to subscribe to shutdown_sender)
        let mut important_background_handles = FuturesUnordered::new();

        // Spawn the influxdb
        let influxdb_client = match top_config.app.influxdb_host.as_ref() {
            Some(influxdb_host) => {
                let influxdb_org = top_config
                    .app
                    .influxdb_org
                    .clone()
                    .expect("influxdb_org needed when influxdb_host is set");
                let influxdb_token = top_config
                    .app
                    .influxdb_token
                    .clone()
                    .expect("influxdb_token needed when influxdb_host is set");

                let influxdb_client =
                    influxdb2::Client::new(influxdb_host, influxdb_org, influxdb_token);

                // TODO: test the client now. having a stat for "started" can be useful on graphs to mark deploys

                Some(influxdb_client)
            }
            None => None,
        };

        // Spawn the stat-sender
        let stat_sender = if let Some(emitter_spawn) = StatBuffer::try_spawn(
            top_config.app.chain_id,
            top_config
                .app
                .influxdb_bucket
                .clone()
                .context("No influxdb bucket was provided")?
                .to_owned(),
            Some(db_conn.clone()),
            influxdb_client.clone(),
            30,
            1,
            BILLING_PERIOD_SECONDS,
            rpc_account_shutdown_recevier,
        )? {
            // since the database entries are used for accounting, we want to be sure everything is saved before exiting
            important_background_handles.push(emitter_spawn.background_handle);

            Some(emitter_spawn.stat_sender)
        } else {
            None
        };

        let migration_timestamp = chrono::offset::Utc::now();

        // Iterate over rows that were not market as "migrated" yet and process them
        loop {
            // (1) Load a batch of rows out of the old table until no more rows are left
            let old_records = rpc_accounting::Entity::find()
                .filter(rpc_accounting::Column::Migrated.is_null())
                .limit(number_of_rows_to_process_at_once)
                .order_by_asc(rpc_accounting::Column::Id)
                .all(db_conn)
                .await?;
            if old_records.len() == 0 {
                // Break out of while loop once all records have successfully been migrated ...
                info!("All records seem to have been successfully migrated!");
                break;
            }

            // (2) Create request metadata objects to match the old data
            // Iterate through all old rows, and put them into the above objects.
            for x in old_records.iter() {
                let authorization_checks = match x.rpc_key_id {
                    Some(rpc_key_id) => {
                        let rpc_key_obj = rpc_key::Entity::find()
                            .filter(rpc_key::Column::Id.eq(rpc_key_id))
                            .one(db_conn)
                            .await?
                            .context("Could not find rpc_key_obj for the given rpc_key_id")?;

                        // TODO: Create authrization
                        // We can probably also randomly generate this, as we don't care about the user (?)
                        AuthorizationChecks {
                            user_id: rpc_key_obj.user_id,
                            rpc_secret_key: Some(RpcSecretKey::Uuid(rpc_key_obj.secret_key)),
                            rpc_secret_key_id: Some(
                                NonZeroU64::new(rpc_key_id)
                                    .context("Could not use rpc_key_id to create a u64")?,
                            ),
                            ..Default::default()
                        }
                    }
                    None => Default::default(),
                };

                let authorization_type = AuthorizationType::Internal;
                let authorization = Arc::new(
                    Authorization::try_new(
                        authorization_checks,
                        None,
                        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                        None,
                        None,
                        None,
                        authorization_type,
                    )
                    .context("Initializing Authorization Struct was not successful")?,
                );

                // It will be like a fork basically (to simulate getting multiple single requests ...)
                // Iterate through all frontend requests
                // For each frontend request, create one object that will be emitted (make sure the timestamp is new)
                let n = x.frontend_requests;

                for i in 0..n {
                    // info!("Creating a new frontend request");

                    // Collect all requests here ...
                    let mut int_request_bytes = (x.sum_request_bytes / n);
                    if i == 0 {
                        int_request_bytes += (x.sum_request_bytes % n);
                    }

                    let mut int_response_bytes = (x.sum_response_bytes / n);
                    if i == 0 {
                        int_response_bytes += (x.sum_response_bytes % n);
                    }

                    let mut int_response_millis = (x.sum_response_millis / n);
                    if i == 0 {
                        int_response_millis += (x.sum_response_millis % n);
                    }

                    let mut int_backend_requests = (x.backend_requests / n);
                    if i == 0 {
                        int_backend_requests += (x.backend_requests % n);
                    }

                    // Add module at the last step to include for any remained that we missed ... (?)

                    // TODO: Create RequestMetadata
                    let request_metadata = RequestMetadata {
                        start_instant: Instant::now(),           // This is overwritten later on
                        request_bytes: int_request_bytes.into(), // Get the mean of all the request bytes
                        archive_request: x.archive_request.into(),
                        backend_requests: Default::default(), // This is not used, instead we modify the field later
                        no_servers: 0.into(), // This is not relevant in the new version
                        error_response: x.error_response.into(),
                        response_bytes: int_response_bytes.into(),
                        response_millis: int_response_millis.into(),
                        // We just don't have this data
                        response_from_backup_rpc: false.into(), // I think we did not record this back then // Default::default()
                    };

                    // (3) Send through a channel to a stat emitter
                    // Send it to the stats sender
                    if let Some(stat_sender_ref) = stat_sender.as_ref() {
                        // info!("Method is: {:?}", x.clone().method);
                        let mut response_stat = RpcQueryStats::new(
                            x.clone().method,
                            authorization.clone(),
                            Arc::new(request_metadata),
                            (int_response_bytes)
                                .try_into()
                                .context("sum bytes average is not calculated properly")?,
                        );
                        // Modify the timestamps ..
                        response_stat.modify_struct(
                            int_response_millis,
                            x.period_datetime.timestamp(),
                            int_backend_requests,
                        );
                        // info!("Sending stats: {:?}", response_stat);
                        stat_sender_ref
                            // .send(response_stat.into())
                            .send_async(response_stat.into())
                            .await
                            .context("stat_sender sending response_stat")?;
                    } else {
                        panic!("Stat sender was not spawned!");
                    }
                }
            }

            // (3) Await that all items are properly processed
            // TODO: Await all the background handles

            // Only after this mark all the items as processed / completed

            // If the items are in rpc_v2, delete the initial items from the database

            // return Ok(());

            // (4) Update the batch in the old table with the current timestamp (Mark the batch as migrated)
            let old_record_ids = old_records.iter().map(|x| x.id);
            let update_result: UpdateResult = rpc_accounting::Entity::update_many()
                .col_expr(
                    rpc_accounting::Column::Migrated,
                    Expr::value(Value::ChronoDateTimeUtc(Some(Box::new(
                        migration_timestamp,
                    )))),
                )
                .filter(rpc_accounting::Column::Id.is_in(old_record_ids))
                // .set(pear)
                .exec(db_conn)
                .await?;

            info!("Update result is: {:?}", update_result);
        }

        info!(
            "Background handles (2) are: {:?}",
            important_background_handles
        );

        drop(stat_sender);

        match app_shutdown_sender.send(()) {
            Err(x) => {
                panic!("Could not send shutdown signal! {:?}", x);
            }
            _ => {}
        };

        // Wait for any tasks that are on-going
        while let Some(x) = important_background_handles.next().await {
            info!("Returned item is: {:?}", x);
            match x {
                Err(e) => {
                    error!("{:?}", e);
                }
                Ok(Err(e)) => {
                    error!("{:?}", e);
                }
                Ok(Ok(_)) => {
                    // TODO: how can we know which handle exited?
                    info!("a background handle exited");
                    // Pop it in this case?
                    continue;
                }
            }
        }
        Ok(())
    }
}
