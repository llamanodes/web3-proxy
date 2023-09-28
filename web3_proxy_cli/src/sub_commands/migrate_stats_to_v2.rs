use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info};
use web3_proxy::app::BILLING_PERIOD_SECONDS;
use web3_proxy::config::TopConfig;
use web3_proxy::frontend::authorization::{
    Authorization, RequestOrMethod, RpcSecretKey, Web3Request,
};
use web3_proxy::prelude::anyhow::{self, Context};
use web3_proxy::prelude::argh::{self, FromArgs};
use web3_proxy::prelude::chrono;
use web3_proxy::prelude::entities::{rpc_accounting, rpc_key};
use web3_proxy::prelude::futures::stream::FuturesUnordered;
use web3_proxy::prelude::futures::StreamExt;
use web3_proxy::prelude::influxdb2;
use web3_proxy::prelude::migration::sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
    UpdateResult,
};
use web3_proxy::prelude::migration::{Expr, Value};
use web3_proxy::prelude::moka::future::Cache;
use web3_proxy::prelude::parking_lot::Mutex;
use web3_proxy::prelude::tokio::sync::{broadcast, mpsc};
use web3_proxy::prelude::tokio::time::Instant;
use web3_proxy::rpcs::one::Web3Rpc;
use web3_proxy::stats::StatBuffer;

#[derive(FromArgs, PartialEq, Eq, Debug)]
/// Migrate towards influxdb and rpc_accounting_v2 from rpc_accounting
#[argh(subcommand, name = "migrate_stats_to_v2")]
pub struct MigrateStatsToV2SubCommand {}

impl MigrateStatsToV2SubCommand {
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

                top_config
                    .app
                    .influxdb_bucket
                    .as_ref()
                    .expect("influxdb_token needed when influxdb_host is set");

                let influxdb_client =
                    influxdb2::Client::new(influxdb_host, influxdb_org, influxdb_token);

                // TODO: test the client now. having a stat for "started" can be useful on graphs to mark deploys

                Some(influxdb_client)
            }
            None => None,
        };

        let (flush_sender, flush_receiver) = mpsc::channel(1);

        let rpc_secret_key_cache = Cache::builder().build();
        let user_balance_cache = Cache::builder().build().into();

        // Spawn the stat-sender
        let emitter_spawn = StatBuffer::try_spawn(
            BILLING_PERIOD_SECONDS,
            top_config.app.chain_id,
            60,
            top_config.app.influxdb_bucket.clone(),
            influxdb_client.clone(),
            rpc_secret_key_cache,
            user_balance_cache,
            rpc_account_shutdown_recevier,
            60,
            flush_sender,
            flush_receiver,
            top_config.app.unique_id,
        )
        .context("Error spawning stat buffer")?
        .context("No stat buffer spawned. Maybe missing influx or db credentials?")?;

        // since the database entries are used for accounting, we want to be sure everything is saved before exiting
        important_background_handles.push(emitter_spawn.background_handle);

        let stat_sender = emitter_spawn.stat_sender;

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
            if old_records.is_empty() {
                // Break out of while loop once all records have successfully been migrated ...
                info!("All records seem to have been successfully migrated!");
                break;
            }

            // (2) Create request metadata objects to match the old data
            // Iterate through all old rows, and put them into the above objects.
            for x in old_records.iter() {
                let mut authorization =
                    Authorization::internal().context("failed creating internal authorization")?;

                match x.rpc_key_id {
                    Some(rpc_key_id) => {
                        let rpc_key_obj = rpc_key::Entity::find()
                            .filter(rpc_key::Column::Id.eq(rpc_key_id))
                            .one(db_conn)
                            .await?
                            .context("Could not find rpc_key_obj for the given rpc_key_id")?;

                        authorization.checks.user_id = rpc_key_obj.user_id;
                        authorization.checks.rpc_secret_key =
                            Some(RpcSecretKey::Uuid(rpc_key_obj.secret_key));
                        authorization.checks.rpc_secret_key_id =
                            NonZeroU64::try_from(rpc_key_id).ok();
                    }
                    None => Default::default(),
                };

                let authorization = Arc::new(authorization);

                // It will be like a fork basically (to simulate getting multiple single requests ...)
                // Iterate through all frontend requests
                // For each frontend request, create one object that will be emitted (make sure the timestamp is new)
                let n = x.frontend_requests;

                for i in 0..n {
                    // info!("Creating a new frontend request");

                    // Collect all requests here ...
                    let mut int_request_bytes = x.sum_request_bytes / n;
                    if i == 0 {
                        int_request_bytes += x.sum_request_bytes % n;
                    }

                    let mut int_response_bytes = x.sum_response_bytes / n;
                    if i == 0 {
                        int_response_bytes += x.sum_response_bytes % n;
                    }

                    let mut int_response_millis = x.sum_response_millis / n;
                    if i == 0 {
                        int_response_millis += x.sum_response_millis % n;
                    }

                    let mut int_backend_requests = x.backend_requests / n;
                    if i == 0 {
                        int_backend_requests += x.backend_requests % n;
                    }

                    // Add module at the last step to include for any remained that we missed ... (?)

                    let backend_rpcs: Vec<_> = (0..int_backend_requests)
                        .map(|_| Arc::new(Web3Rpc::default()))
                        .collect();

                    let chain_id = x.chain_id;

                    let method = x
                        .method
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string())
                        .into();

                    let request = RequestOrMethod::Method(method, int_request_bytes as usize);

                    // cache isn't needed on this
                    let cache_key = None;

                    // Create Web3Request
                    let web3_request = Web3Request {
                        archive_request: x.archive_request.into(),
                        authorization: authorization.clone(),
                        backend_requests: Mutex::new(backend_rpcs),
                        cache_key,
                        chain_id,
                        error_response: x.error_response.into(),
                        head_block: None,
                        // debug data is in kafka, not mysql or influx
                        kafka_debug_logger: None,
                        request,
                        // This is not relevant in the new version
                        no_servers: 0.into(),
                        response_bytes: int_response_bytes.into(),
                        // We did not initially record this data
                        response_from_backup_rpc: false.into(),
                        response_timestamp: x.period_datetime.timestamp().into(),
                        response_millis: int_response_millis.into(),
                        // This is overwritten later on
                        start_instant: Instant::now(),
                        expire_instant: Instant::now() + Duration::from_secs(295),
                        stat_sender: Some(stat_sender.clone()),
                        user_error_response: false.into(),
                        usd_per_cu: top_config.app.usd_per_cu.unwrap_or_default(),
                    };

                    web3_request.try_send_stat()?;
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

        if let Err(x) = app_shutdown_sender.send(()) {
            panic!("Could not send shutdown signal! {:?}", x);
        };

        // Wait for any tasks that are on-going
        while let Some(x) = important_background_handles.next().await {
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
                }
            }
        }
        Ok(())
    }
}
