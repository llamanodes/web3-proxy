mod change_user_tier_by_key;
mod check_config;
mod create_user;
mod drop_migration_lock;

use argh::FromArgs;
use std::fs;
use web3_proxy::{
    app::{get_db, get_migrated_db},
    config::TopConfig,
};

#[derive(Debug, FromArgs)]
/// Command line interface for admins to interact with web3_proxy
pub struct CliConfig {
    /// path to the application config (optional).
    #[argh(option)]
    pub config: Option<String>,

    /// if no config, what database the client should connect to. Defaults to dev db.
    #[argh(
        option,
        default = "\"mysql://root:dev_web3_proxy@127.0.0.1:13306/dev_web3_proxy\".to_string()"
    )]
    pub db_url: String,

    /// this one cli can do multiple things
    #[argh(subcommand)]
    sub_command: SubCommand,
}

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand)]
enum SubCommand {
    ChangeUserTierByKey(change_user_tier_by_key::ChangeUserTierByKeyCommand),
    CheckConfig(check_config::CheckConfigSubCommand),
    CreateUser(create_user::CreateUserSubCommand),
    DropMigrationLock(drop_migration_lock::DropMigrationLockSubCommand),
    // TODO: sub command to downgrade migrations?
    // TODO: sub command to add new api keys to an existing user?
    // TODO: sub command to change a user's tier
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // if RUST_LOG isn't set, configure a default
    // TODO: is there a better way to do this?
    if std::env::var("RUST_LOG").is_err() {
        // std::env::set_var("RUST_LOG", "info,web3_proxy=debug,web3_proxy_cli=debug");
        std::env::set_var("RUST_LOG", "info,web3_proxy=debug,web3_proxy_cli=debug");
    }

    env_logger::init();

    // this probably won't matter for us in docker, but better safe than sorry
    fdlimit::raise_fd_limit();

    let mut cli_config: CliConfig = argh::from_env();

    let _top_config = if let Some(top_config_path) = cli_config.config.clone() {
        let top_config: String = fs::read_to_string(top_config_path)?;
        let top_config: TopConfig = toml::from_str(&top_config)?;

        if let Some(top_config_db_url) = top_config.app.db_url.clone() {
            cli_config.db_url = top_config_db_url;
        }

        Some(top_config)
    } else {
        None
    };

    match cli_config.sub_command {
        SubCommand::ChangeUserTierByKey(x) => {
            let db_conn = get_db(cli_config.db_url, 1, 1).await?;

            x.main(&db_conn).await
        }
        SubCommand::CheckConfig(x) => x.main().await,
        SubCommand::CreateUser(x) => {
            let db_conn = get_migrated_db(cli_config.db_url, 1, 1).await?;

            x.main(&db_conn).await
        }
        SubCommand::DropMigrationLock(x) => {
            let db_conn = get_db(cli_config.db_url, 1, 1).await?;

            x.main(&db_conn).await
        }
    }
}
