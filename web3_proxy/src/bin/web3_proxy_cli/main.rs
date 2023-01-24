mod change_user_address;
mod change_user_tier;
mod change_user_tier_by_address;
mod change_user_tier_by_key;
mod check_config;
mod count_users;
mod create_user;
mod daemon;
mod drop_migration_lock;
mod list_user_tier;
mod pagerduty;
mod rpc_accounting;
mod sentryd;
mod transfer_key;
mod user_export;
mod user_import;

use anyhow::Context;
use argh::FromArgs;
use ethers::types::U256;
use gethostname::gethostname;
use log::{error, info, warn};
use pagerduty_rs::eventsv2sync::EventsV2 as PagerdutySyncEventsV2;
use pagerduty_rs::types::{AlertTrigger, AlertTriggerPayload};
use pagerduty_rs::{eventsv2async::EventsV2 as PagerdutyAsyncEventsV2, types::Event};
use std::{
    fs, panic,
    path::Path,
    sync::atomic::{self, AtomicUsize},
};
use tokio::runtime;
use web3_proxy::{
    app::{get_db, get_migrated_db, APP_USER_AGENT},
    config::TopConfig,
};

#[cfg(feature = "deadlock")]
use parking_lot::deadlock;
#[cfg(feature = "deadlock")]
use std::thread;
#[cfg(feature = "deadlock")]
use tokio::time::Duration;

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
    ChangeUserAddress(change_user_address::ChangeUserAddressSubCommand),
    ChangeUserTier(change_user_tier::ChangeUserTierSubCommand),
    ChangeUserTierByAddress(change_user_tier_by_address::ChangeUserTierByAddressSubCommand),
    ChangeUserTierByKey(change_user_tier_by_key::ChangeUserTierByKeySubCommand),
    CheckConfig(check_config::CheckConfigSubCommand),
    CountUsers(count_users::CountUsersSubCommand),
    CreateUser(create_user::CreateUserSubCommand),
    DropMigrationLock(drop_migration_lock::DropMigrationLockSubCommand),
    Pagerduty(pagerduty::PagerdutySubCommand),
    Proxyd(daemon::ProxydSubCommand),
    RpcAccounting(rpc_accounting::RpcAccountingSubCommand),
    Sentryd(sentryd::SentrydSubCommand),
    TransferKey(transfer_key::TransferKeySubCommand),
    UserExport(user_export::UserExportSubCommand),
    UserImport(user_import::UserImportSubCommand),
    // TODO: sub command to downgrade migrations? sea-orm has this but doing downgrades here would be easier+safer
    // TODO: sub command to add new api keys to an existing user?
    // TODO: sub command to change a user's tier
}

fn main() -> anyhow::Result<()> {
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
    let rust_log = match std::env::var("RUST_LOG") {
        Ok(x) => x,
        Err(_) => "info,ethers=debug,redis_rate_limit=debug,web3_proxy=debug,web3_proxy_cli=debug"
            .to_string(),
    };

    // this probably won't matter for us in docker, but better safe than sorry
    fdlimit::raise_fd_limit();

    let mut cli_config: Web3ProxyCli = argh::from_env();

    if cli_config.config.is_none() && cli_config.db_url.is_none() && cli_config.sentry_url.is_none()
    {
        // TODO: default to example.toml if development.toml doesn't exist
        info!("defaulting to development config");
        cli_config.config = Some("./config/development.toml".to_string());
    }

    let top_config = if let Some(top_config_path) = cli_config.config.clone() {
        let top_config_path = Path::new(&top_config_path)
            .canonicalize()
            .context(format!("checking for config at {}", top_config_path))?;

        let top_config: String = fs::read_to_string(top_config_path)?;
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

        Some(top_config)
    } else {
        None
    };

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

    // panic handler that sends to pagerduty
    // TODO: there is a `pagerduty_panic` module that looks like it would work with minor tweaks, but ethers-rs panics when a websocket exit and that would fire too many alerts

    if let Some(pagerduty_sync) = pagerduty_sync {
        let client = top_config
            .as_ref()
            .map(|top_config| format!("web3-proxy chain #{}", top_config.app.chain_id))
            .unwrap_or_else(|| format!("web3-proxy w/o chain"));

        let client_url = top_config
            .as_ref()
            .and_then(|x| x.app.redirect_public_url.clone());

        panic::set_hook(Box::new(move |x| {
            let hostname = gethostname().into_string().unwrap_or("unknown".to_string());
            let panic_msg = format!("{} {:?}", x, x);

            error!("sending panic to pagerduty: {}", panic_msg);

            let payload = AlertTriggerPayload {
                severity: pagerduty_rs::types::Severity::Error,
                summary: panic_msg.clone(),
                source: hostname,
                timestamp: None,
                component: None,
                group: Some("web3-proxy".to_string()),
                class: Some("panic".to_string()),
                custom_details: None::<()>,
            };

            let event = Event::AlertTrigger(AlertTrigger {
                payload,
                dedup_key: None,
                images: None,
                links: None,
                client: Some(client.clone()),
                client_url: client_url.clone(),
            });

            if let Err(err) = pagerduty_sync.event(event) {
                error!("Failed sending panic to pagerduty: {}", err);
            }
        }));
    } else {
        info!("No pagerduty key. Using default panic handler");
    }

    // set up tokio's async runtime
    let mut rt_builder = runtime::Builder::new_multi_thread();

    rt_builder.enable_all();

    if let Some(top_config) = top_config.as_ref() {
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
            SubCommand::ChangeUserAddress(x) => {
                let db_url = cli_config
                    .db_url
                    .expect("'--config' (with a db) or '--db-url' is required to run proxyd");

                let db_conn = get_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
            SubCommand::ChangeUserTier(x) => {
                let db_url = cli_config
                    .db_url
                    .expect("'--config' (with a db) or '--db-url' is required to run proxyd");

                let db_conn = get_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
            SubCommand::ChangeUserTierByAddress(x) => {
                let db_url = cli_config
                    .db_url
                    .expect("'--config' (with a db) or '--db-url' is required to run proxyd");

                let db_conn = get_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
            SubCommand::ChangeUserTierByKey(x) => {
                let db_url = cli_config
                    .db_url
                    .expect("'--config' (with a db) or '--db-url' is required to run proxyd");

                let db_conn = get_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
            SubCommand::CheckConfig(x) => x.main().await,
            SubCommand::CreateUser(x) => {
                let db_url = cli_config
                    .db_url
                    .expect("'--config' (with a db) or '--db-url' is required to run proxyd");

                let db_conn = get_migrated_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
            SubCommand::CountUsers(x) => {
                let db_url = cli_config
                    .db_url
                    .expect("'--config' (with a db) or '--db-url' is required to run proxyd");

                let db_conn = get_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
            SubCommand::Proxyd(x) => {
                let top_config = top_config.expect("--config is required to run proxyd");

                x.main(top_config, num_workers).await
            }
            SubCommand::DropMigrationLock(x) => {
                let db_url = cli_config
                    .db_url
                    .expect("'--config' (with a db) or '--db-url' is required to run proxyd");

                // very intentionally, do NOT run migrations here
                let db_conn = get_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
            SubCommand::Pagerduty(x) => {
                if cli_config.sentry_url.is_none() {
                    warn!("sentry_url is not set! Logs will only show in this console");
                }

                x.main(pagerduty_async, top_config).await
            }
            SubCommand::Sentryd(x) => {
                if cli_config.sentry_url.is_none() {
                    warn!("sentry_url is not set! Logs will only show in this console");
                }

                x.main().await
            }
            SubCommand::RpcAccounting(x) => {
                let db_url = cli_config
                    .db_url
                    .expect("'--config' (with a db) or '--db-url' is required to run proxyd");

                let db_conn = get_migrated_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
            SubCommand::TransferKey(x) => {
                let db_url = cli_config
                    .db_url
                    .expect("'--config' (with a db) or '--db-url' is required to run proxyd");
                let db_conn = get_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
            SubCommand::UserExport(x) => {
                let db_url = cli_config
                    .db_url
                    .expect("'--config' (with a db) or '--db-url' is required to run proxyd");

                let db_conn = get_migrated_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
            SubCommand::UserImport(x) => {
                let db_url = cli_config
                    .db_url
                    .expect("'--config' (with a db) or '--db-url' is required to run proxyd");

                let db_conn = get_migrated_db(db_url, 1, 1).await?;

                x.main(&db_conn).await
            }
        }
    })
}
