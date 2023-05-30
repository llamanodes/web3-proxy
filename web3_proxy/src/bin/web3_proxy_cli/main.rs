mod change_admin_status;
mod change_user_address;
mod change_user_tier;
mod change_user_tier_by_address;
mod change_user_tier_by_key;
mod check_config;
mod count_users;
mod create_key;
mod create_user;
mod drop_migration_lock;
mod list_user_tier;
mod migrate_stats_to_v2;
mod pagerduty;
mod popularity_contest;
mod proxyd;
mod rpc_accounting;
mod search_kafka;
mod sentryd;
mod transfer_key;
mod user_export;
mod user_import;

use anyhow::Context;
use argh::FromArgs;
use ethers::types::U256;
use log::{info, warn};
use pagerduty_rs::eventsv2async::EventsV2 as PagerdutyAsyncEventsV2;
use pagerduty_rs::eventsv2sync::EventsV2 as PagerdutySyncEventsV2;
use std::{
    fs, panic,
    path::Path,
    sync::atomic::{self, AtomicUsize},
};
use tokio::runtime;
use web3_proxy::pagerduty::panic_handler;
use web3_proxy::{
    app::{get_db, get_migrated_db, APP_USER_AGENT},
    config::TopConfig,
};

#[cfg(feature = "mimalloc")]
use mimalloc::MiMalloc;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[cfg(feature = "deadlock")]
use {parking_lot::deadlock, std::thread, tokio::time::Duration};

#[derive(Debug, FromArgs)]
/// Command line interface for admins to interact with web3_proxy
pub struct Web3ProxyCli {
    /// path to the application config (only required for some commands; defaults to dev config).
    #[argh(option)]
    pub config: Option<String>,

    /// number of worker threads. Defaults to the number of logical processors
    #[argh(option, default = "0")]
    pub workers: usize,

    /// if no config, what database the client should connect to (only required for some commands; Defaults to dev db)
    #[argh(option)]
    pub db_url: Option<String>,

    /// if no config, what sentry url should the client should connect to
    #[argh(option)]
    pub sentry_url: Option<String>,

    /// this one cli can do multiple things
    #[argh(subcommand)]
    sub_command: SubCommand,
}

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand)]
enum SubCommand {
    ChangeAdminStatus(change_admin_status::ChangeAdminStatusSubCommand),
    ChangeUserAddress(change_user_address::ChangeUserAddressSubCommand),
    ChangeUserTier(change_user_tier::ChangeUserTierSubCommand),
    ChangeUserTierByAddress(change_user_tier_by_address::ChangeUserTierByAddressSubCommand),
    ChangeUserTierByKey(change_user_tier_by_key::ChangeUserTierByKeySubCommand),
    CheckConfig(check_config::CheckConfigSubCommand),
    CountUsers(count_users::CountUsersSubCommand),
    CreateKey(create_key::CreateKeySubCommand),
    CreateUser(create_user::CreateUserSubCommand),
    DropMigrationLock(drop_migration_lock::DropMigrationLockSubCommand),
    MigrateStatsToV2(migrate_stats_to_v2::MigrateStatsToV2),
    Pagerduty(pagerduty::PagerdutySubCommand),
    PopularityContest(popularity_contest::PopularityContestSubCommand),
    Proxyd(proxyd::ProxydSubCommand),
    RpcAccounting(rpc_accounting::RpcAccountingSubCommand),
    SearchKafka(search_kafka::SearchKafkaSubCommand),
    Sentryd(sentryd::SentrydSubCommand),
    TransferKey(transfer_key::TransferKeySubCommand),
    UserExport(user_export::UserExportSubCommand),
    UserImport(user_import::UserImportSubCommand),
    // TODO: sub command to downgrade migrations? sea-orm has this but doing downgrades here would be easier+safer
    // TODO: sub command to add new api keys to an existing user?
    // TODO: sub command to change a user's tier
}

fn main() -> anyhow::Result<()> {
    // this probably won't matter for us in docker, but better safe than sorry
    fdlimit::raise_fd_limit();

    #[cfg(feature = "deadlock")]
    {
        // spawn a thread for deadlock detection
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(10));
            let deadlocks = deadlock::check_deadlock();
            if deadlocks.is_empty() {
                continue;
            }

            println!("{} deadlocks detected", deadlocks.len());
            for (i, threads) in deadlocks.iter().enumerate() {
                println!("Deadlock #{}", i);
                for t in threads {
                    println!("Thread Id {:#?}", t.thread_id());
                    println!("{:#?}", t.backtrace());
                }
            }
        });
    }

    // if RUST_LOG isn't set, configure a default
    // TODO: is there a better way to do this?
    #[cfg(feature = "tokio_console")]
    console_subscriber::init();

    #[cfg(not(feature = "tokio_console"))]
    let rust_log = match std::env::var("RUST_LOG") {
        Ok(x) => x,
        Err(_) => match std::env::var("WEB3_PROXY_TRACE").map(|x| x == "true") {
            Ok(true) => {
                vec![
                    "info",
                    "ethers=debug",
                    "ethers_providers::rpc=off",
                    "ethers_providers=debug",
                    "quick_cache_ttl=debug",
                    "redis_rate_limit=debug",
                    "web3_proxy::rpcs::blockchain=info",
                    "web3_proxy::rpcs::request=debug",
                    // "web3_proxy::stats::influxdb_queries=trace",
                    "web3_proxy=trace",
                    "web3_proxy_cli=trace",
                ]
            }
            _ => {
                vec![
                    "info",
                    "ethers=debug",
                    "ethers_providers::rpc=off",
                    "ethers_providers=error",
                    "quick_cache_ttl=info",
                    "redis_rate_limit=debug",
                    // "web3_proxy::stats::influxdb_queries=trace",
                    "web3_proxy=debug",
                    "web3_proxy_cli=debug",
                ]
            }
        }
        .join(","),
    };

    let mut cli_config: Web3ProxyCli = argh::from_env();

    if cli_config.config.is_none() && cli_config.db_url.is_none() && cli_config.sentry_url.is_none()
    {
        // TODO: default to example.toml if development.toml doesn't exist
        info!("defaulting to development config");
        cli_config.config = Some("./config/development.toml".to_string());
    }

    let (top_config, top_config_path) = if let Some(top_config_path) = cli_config.config.clone() {
        let top_config_path = Path::new(&top_config_path)
            .canonicalize()
            .context(format!("checking for config at {}", top_config_path))?;

        let top_config: String = fs::read_to_string(top_config_path.clone())?;

        let mut top_config: TopConfig = toml::from_str(&top_config)?;

        // TODO: this doesn't seem to do anything
        proctitle::set_title(format!("web3_proxy-{}", top_config.app.chain_id));

        if cli_config.db_url.is_none() {
            cli_config.db_url = top_config.app.db_url.clone();
        }

        if let Some(sentry_url) = top_config.app.sentry_url.clone() {
            cli_config.sentry_url = Some(sentry_url);
        }

        if top_config.app.chain_id == 137 {
            // TODO: these numbers are arbitrary. i think the maticnetwork/erigon fork has a bug
            if top_config.app.gas_increase_min.is_none() {
                top_config.app.gas_increase_min = Some(U256::from(40_000));
            }

            if top_config.app.gas_increase_percent.is_none() {
                top_config.app.gas_increase_percent = Some(U256::from(40));
            }
        }

        (Some(top_config), Some(top_config_path))
    } else {
        (None, None)
    };

    {
        let logger = env_logger::builder().parse_filters(&rust_log).build();

        let max_level = logger.filter();

        // connect to sentry for error reporting
        // if no sentry, only log to stdout
        let _sentry_guard = if let Some(sentry_url) = cli_config.sentry_url.clone() {
            let logger = sentry::integrations::log::SentryLogger::with_dest(logger);

            log::set_boxed_logger(Box::new(logger)).unwrap();

            let guard = sentry::init((
                sentry_url,
                sentry::ClientOptions {
                    release: sentry::release_name!(),
                    // TODO: Set this a to lower value (from config) in production
                    traces_sample_rate: 1.0,
                    ..Default::default()
                },
            ));

            Some(guard)
        } else {
            log::set_boxed_logger(Box::new(logger)).unwrap();

            None
        };

        log::set_max_level(max_level);

        info!("RUST_LOG={}", rust_log);
    }

    info!("{}", APP_USER_AGENT);

    // optionally connect to pagerduty
    // TODO: fix this nested result
    let (pagerduty_async, pagerduty_sync) = if let Ok(pagerduty_key) =
        std::env::var("PAGERDUTY_INTEGRATION_KEY")
    {
        let pagerduty_async =
            PagerdutyAsyncEventsV2::new(pagerduty_key.clone(), Some(APP_USER_AGENT.to_string()))?;
        let pagerduty_sync =
            PagerdutySyncEventsV2::new(pagerduty_key, Some(APP_USER_AGENT.to_string()))?;

        (Some(pagerduty_async), Some(pagerduty_sync))
    } else {
        info!("No PAGERDUTY_INTEGRATION_KEY");

        (None, None)
    };

    // panic handler that sends to pagerduty.
    // TODO: use the sentry handler if no pager duty. use default if no sentry
    if let Some(pagerduty_sync) = pagerduty_sync {
        let top_config = top_config.clone();

        panic::set_hook(Box::new(move |x| {
            panic_handler(top_config.clone(), &pagerduty_sync, x);
        }));
    }

    // set up tokio's async runtime
    let mut rt_builder = runtime::Builder::new_multi_thread();

    rt_builder.enable_all();

    if cli_config.workers > 0 {
        rt_builder.worker_threads(cli_config.workers);
    }

    if let Some(ref top_config) = top_config {
        let chain_id = top_config.app.chain_id;

        rt_builder.thread_name_fn(move || {
            static ATOMIC_ID: AtomicUsize = AtomicUsize::new(0);
            // TODO: what ordering? i think we want seqcst so that these all happen in order, but that might be stricter than we really need
            let worker_id = ATOMIC_ID.fetch_add(1, atomic::Ordering::SeqCst);
            // TODO: i think these max at 15 characters
            format!("web3-{}-{}", chain_id, worker_id)
        });
    }

    // start tokio's async runtime
    let rt = rt_builder.build()?;

    let num_workers = rt.metrics().num_workers();
    info!("num_workers: {}", num_workers);

    rt.block_on(async {
        match cli_config.sub_command {
            SubCommand::ChangeAdminStatus(x) => {
                let db_url = cli_config.db_url.expect(
                    "'--config' (with a db) or '--db-url' is required to run change_admin_status",
                );

                let db_conn = get_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
            SubCommand::ChangeUserAddress(x) => {
                let db_url = cli_config
                    .db_url
                    .expect("'--config' (with a db) or '--db-url' is required to run change_user_addres");

                let db_conn = get_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
            SubCommand::ChangeUserTier(x) => {
                let db_url = cli_config
                    .db_url
                    .expect("'--config' (with a db) or '--db-url' is required to run change_user_tier");

                let db_conn = get_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
            SubCommand::ChangeUserTierByAddress(x) => {
                let db_url = cli_config.db_url.expect(
                    "'--config' (with a db) or '--db-url' is required to run change_user_tier_by_address",
                );

                let db_conn = get_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
            SubCommand::ChangeUserTierByKey(x) => {
                let db_url = cli_config
                    .db_url
                    .expect("'--config' (with a db) or '--db-url' is required to run change_user_tier_by_key");

                let db_conn = get_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
            SubCommand::CheckConfig(x) => x.main().await,
            SubCommand::CreateKey(x) => {
                let db_url = cli_config
                    .db_url
                    .expect("'--config' (with a db) or '--db-url' is required to run create a key");

                let db_conn = get_migrated_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
            SubCommand::CreateUser(x) => {
                let db_url = cli_config
                    .db_url
                    .expect("'--config' (with a db) or '--db-url' is required to run create_user");

                let db_conn = get_migrated_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
            SubCommand::CountUsers(x) => {
                let db_url = cli_config
                    .db_url
                    .expect("'--config' (with a db) or '--db-url' is required to run count_users");

                let db_conn = get_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
            SubCommand::Proxyd(x) => {
                let top_config = top_config.expect("--config is required to run proxyd");
                let top_config_path =
                    top_config_path.expect("path must be set if top_config exists");

                x.main(top_config, top_config_path, num_workers).await
            }
            SubCommand::DropMigrationLock(x) => {
                let db_url = cli_config
                    .db_url
                    .expect("'--config' (with a db) or '--db-url' is required to run drop_migration_lock");

                // very intentionally, do NOT run migrations here. that would wait forever if the migration lock is abandoned
                let db_conn = get_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
            SubCommand::MigrateStatsToV2(x) => {

                let top_config = top_config.expect("--config is required to run the migration from stats-mysql to stats-influx");
                // let top_config_path =
                //     top_config_path.expect("path must be set if top_config exists");

                let db_url = cli_config
                    .db_url
                    .expect("'--config' (with a db) or '--db-url' is required to run the migration from stats-mysql to stats-influx");

                let db_conn = get_db(db_url, 1, 1).await?;
                x.main(top_config, &db_conn).await
            }
            SubCommand::Pagerduty(x) => {
                if cli_config.sentry_url.is_none() {
                    warn!("sentry_url is not set! Logs will only show in this console");
                }

                x.main(pagerduty_async, top_config).await
            }
            SubCommand::PopularityContest(x) => x.main().await,
            SubCommand::SearchKafka(x) => x.main(top_config.unwrap()).await,
            SubCommand::Sentryd(x) => {
                if cli_config.sentry_url.is_none() {
                    warn!("sentry_url is not set! Logs will only show in this console");
                }

                x.main(pagerduty_async, top_config).await
            }
            SubCommand::RpcAccounting(x) => {
                let db_url = cli_config
                    .db_url
                    .expect("'--config' (with a db) or '--db-url' is required to run rpc_accounting");

                let db_conn = get_migrated_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
            SubCommand::TransferKey(x) => {
                let db_url = cli_config
                    .db_url
                    .expect("'--config' (with a db) or '--db-url' is required to run transfer_key");
                let db_conn = get_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
            SubCommand::UserExport(x) => {
                let db_url = cli_config
                    .db_url
                    .expect("'--config' (with a db) or '--db-url' is required to run user_export");

                let db_conn = get_migrated_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
            SubCommand::UserImport(x) => {
                let db_url = cli_config
                    .db_url
                    .expect("'--config' (with a db) or '--db-url' is required to run user_import");

                let db_conn = get_migrated_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
        }
    })
}
