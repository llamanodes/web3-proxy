use argh::FromArgs;
use entities::{user, user_keys};
use fstrings::{format_args_f, println_f};
use sea_orm::ActiveModelTrait;
use web3_proxy::users::new_api_key;

#[derive(Debug, FromArgs)]
/// Command line interface for admins to interact with web3-proxy
pub struct TopConfig {
    /// what host the client should connect to
    #[argh(
        option,
        default = "\"mysql://root:dev_web3_proxy@127.0.0.1:3306/dev_web3_proxy\".to_string()"
    )]
    pub db_url: String,

    /// this one cli can do multiple things
    #[argh(subcommand)]
    sub_command: SubCommand,
}

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand)]
enum SubCommand {
    CreateUser(CreateUserSubCommand),
    Two(SubCommandTwo),
    // TODO: sub command to downgrade migrations?
    // TODO: sub command to add new api keys to an existing user?
}

#[derive(FromArgs, PartialEq, Debug)]
/// First subcommand.
#[argh(subcommand, name = "create_user")]
struct CreateUserSubCommand {
    #[argh(option)]
    /// the user's ethereum address
    address: String,

    #[argh(option)]
    /// the user's optional email
    email: Option<String>,
}

impl CreateUserSubCommand {
    async fn main(self, db: &sea_orm::DatabaseConnection) -> anyhow::Result<()> {
        let u = user::ActiveModel {
            address: sea_orm::Set(self.address),
            email: sea_orm::Set(self.email),
            ..Default::default()
        };

        // TODO: proper error message
        let u = u.insert(db).await?;

        println_f!("user: {u:?}");

        // create a key for the new user
        let uk = user_keys::ActiveModel {
            user_uuid: sea_orm::Set(u.uuid),
            api_key: sea_orm::Set(new_api_key()),
            ..Default::default()
        };

        println_f!("user key: {uk:?}");

        Ok(())
    }
}

#[derive(FromArgs, PartialEq, Debug)]
/// Second subcommand.
#[argh(subcommand, name = "two")]
struct SubCommandTwo {
    #[argh(switch)]
    /// whether to fooey
    fooey: bool,
}

impl SubCommandTwo {
    async fn main(self) -> anyhow::Result<()> {
        todo!()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli_config: TopConfig = argh::from_env();

    println!("hello, {}", cli_config.db_url);

    match cli_config.sub_command {
        SubCommand::CreateUser(x) => {
            // TODO: more advanced settings
            let db_conn = sea_orm::Database::connect(cli_config.db_url).await?;

            x.main(&db_conn).await
        }
        SubCommand::Two(x) => x.main().await,
    }
}
