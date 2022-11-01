use argh::FromArgs;
use std::fs;
use tracing::{info, warn};
use web3_proxy::config::TopConfig;

#[derive(FromArgs, PartialEq, Eq, Debug)]
/// Second subcommand.
#[argh(subcommand, name = "check_config")]
pub struct CheckConfigSubCommand {
    #[argh(positional)]
    /// path to the configuration toml.
    path: String,
}

impl CheckConfigSubCommand {
    pub async fn main(self) -> anyhow::Result<()> {
        info!("Loading config @ {}", self.path);
        let top_config: String = fs::read_to_string(self.path)?;
        let top_config: TopConfig = toml::from_str(&top_config)?;

        // TODO: pretty print
        info!("config: {:#?}", top_config);

        if top_config.app.db_url.is_none() {
            warn!("app.db_url is not set! Some features disabled")
        }

        match top_config.app.public_requests_per_minute {
            None => {
                info!("app.public_requests_per_minute is None. Fully open to public requests!")
            }
            Some(0) => {
                info!("app.public_requests_per_minute is 0. Public requests are blocked!")
            }
            Some(_) => {}
        }

        match top_config.app.default_user_max_requests_per_minute {
            None => {
                info!("app.default_user_requests_per_minute is None. Fully open to registered requests!")
            }
            Some(0) => warn!("app.default_user_requests_per_minute is 0. Registered user's requests are blocked! Are you sure you want that?"),
            Some(_) => {
                // TODO: make sure this isn't < anonymous requests per minute
            }
        }

        match top_config.app.invite_code {
            None => info!("app.invite_code is None. Registration is open"),
            Some(_) => info!("app.invite_code is set. Registration is limited"),
        }

        // TODO: check min_sum_soft_limit is a reasonable amount
        // TODO: check min_synced_rpcs is a reasonable amount
        // TODO: check frontend_rate_limit_per_minute is a reasonable amount. requires redis
        // TODO: check login_rate_limit_per_minute is a reasonable amount. requires redis

        if top_config.app.volatile_redis_url.is_none() {
            warn!("app.volatile_redis_url is not set! Some features disabled")
        }

        // TODO: check response_cache_max_bytes is a reasonable amount

        if top_config.app.redirect_public_url.is_none() {
            warn!("app.redirect_public_url is None. Anonyoumous users will get an error page instead of a redirect")
        }

        if top_config.app.redirect_user_url.is_none() {
            warn!("app.redirect_public_url is None. Registered users will get an error page instead of a redirect")
        }

        Ok(())
    }
}
