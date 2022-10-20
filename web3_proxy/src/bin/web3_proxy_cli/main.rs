mod check_config;
mod create_user;

use argh::FromArgs;
use web3_proxy::app::get_migrated_db;

#[derive(Debug, FromArgs)]
/// Command line interface for admins to interact with web3_proxy
pub struct TopConfig {
    /// what database the client should connect to
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
    CreateUser(create_user::CreateUserSubCommand),
    CheckConfig(check_config::CheckConfigSubCommand),
    // TODO: sub command to downgrade migrations?
    // TODO: sub command to add new api keys to an existing user?
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // if RUST_LOG isn't set, configure a default
    // TODO: is there a better way to do this?
    if std::env::var("RUST_LOG").is_err() {
        // std::env::set_var("RUST_LOG", "info,web3_proxy=debug,web3_proxy_cli=debug");
        std::env::set_var("RUST_LOG", "info,web3_proxy=debug,web3_proxy_cli=debug");
    }

    // install global collector configured based on RUST_LOG env var.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .compact()
        .init();

    // this probably won't matter for us in docker, but better safe than sorry
    fdlimit::raise_fd_limit();

    let cli_config: TopConfig = argh::from_env();

    match cli_config.sub_command {
        SubCommand::CreateUser(x) => {
            let db_conn = get_migrated_db(cli_config.db_url, 1, 1).await?;

            x.main(&db_conn).await
        }
        SubCommand::CheckConfig(x) => x.main().await,
    }
}
