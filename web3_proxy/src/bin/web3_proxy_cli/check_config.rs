use argh::FromArgs;
use std::fs;
use tracing::info;
use web3_proxy::config::TopConfig;

#[derive(FromArgs, PartialEq, Eq, Debug)]
/// Second subcommand.
#[argh(subcommand, name = "check_config")]
pub struct CheckConfigSubCommand {
    #[argh(option)]
    /// path to the configuration toml.
    path: String,
}

impl CheckConfigSubCommand {
    pub async fn main(self) -> anyhow::Result<()> {
        info!("Loading config @ {}", self.path);
        let top_config: String = fs::read_to_string(self.path)?;
        let top_config: TopConfig = toml::from_str(&top_config)?;

        info!("config: {:?}", top_config);

        Ok(())
    }
}
