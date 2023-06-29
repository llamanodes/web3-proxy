use crate::config::TopConfig;
use argh::FromArgs;
use std::fs;
use tracing::{error, info, warn};

#[derive(FromArgs, PartialEq, Eq, Debug)]
/// Check the config for any problems.
#[argh(subcommand, name = "check_config")]
pub struct CheckConfigSubCommand {
    #[argh(positional)]
    /// path to the configuration toml.
    path: String,
}

impl CheckConfigSubCommand {
    pub async fn main(self) -> anyhow::Result<()> {
        let mut num_errors = 0;

        info!("Loading config @ {}", self.path);
        let top_config: String = fs::read_to_string(self.path)?;
        let top_config: TopConfig = toml::from_str(&top_config)?;

        // TODO: pretty print
        info!("config: {:#?}", top_config);

        if top_config.app.db_url.is_none() {
            warn!("app.db_url is not set! Some features disabled")
        }

        match top_config.app.public_requests_per_period {
            None => {
                info!("app.public_requests_per_period is None. Fully open to public requests!")
            }
            Some(0) => {
                info!("app.public_requests_per_period is 0. Public requests are blocked!")
            }
            Some(_) => {}
        }

        match top_config.app.default_user_max_requests_per_period {
            None => {
                info!("app.default_user_requests_per_period is None. Fully open to registered requests!")
            }
            Some(0) => warn!("app.default_user_requests_per_period is 0. Registered user's requests are blocked! Are you sure you want that?"),
            Some(_) => {
                // TODO: make sure this isn't < anonymous requests per period
            }
        }

        match top_config.app.invite_code {
            None => info!("app.invite_code is None. Registration is open"),
            Some(_) => info!("app.invite_code is set. Registration is limited"),
        }

        // TODO: check min_sum_soft_limit is a reasonable amount
        // TODO: check min_synced_rpcs is a reasonable amount
        // TODO: check frontend_rate_limit_per_period is a reasonable amount. requires redis
        // TODO: check login_rate_limit_per_period is a reasonable amount. requires redis

        if top_config.app.volatile_redis_url.is_none() {
            warn!("app.volatile_redis_url is not set! Some features disabled")
        }

        // TODO: check response_cache_max_bytes is a reasonable amount

        if top_config.app.redirect_public_url.is_none() {
            warn!("app.redirect_public_url is None. Anonyoumous users will get an error page instead of a redirect")
        }

        // TODO: also check that it contains rpc_key_id!
        match top_config.app.redirect_rpc_key_url {
            None => {
                warn!("app.redirect_rpc_key_url is None. Registered users will get an error page instead of a redirect")
            }
            Some(x) => {
                if !x.contains("{{rpc_key_id}}") {
                    num_errors += 1;
                    error!("redirect_rpc_key_url user url must contain \"{{rpc_key_id}}\"")
                }
            }
        }

        // TODO: print num warnings and have a flag to fail even on warnings

        if num_errors == 0 {
            Ok(())
        } else {
            Err(anyhow::anyhow!("there were {} errors!", num_errors))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::env;

    use super::*;

    #[tokio::test]
    async fn check_example_toml() {
        let path = env::current_dir().expect("path");

        let parent = path.parent().expect("always a parent");

        let config_path = parent.join("config").join("example.toml");

        let config_path_str = config_path.to_str().expect("always a valid path");

        let check_config_command =
            CheckConfigSubCommand::from_args(&["check_config"], &[config_path_str])
                .expect("the command should have run");

        let check_config_result = check_config_command.main().await;

        println!("{:?}", check_config_result);

        check_config_result.expect("the config should pass all checks");
    }
}
