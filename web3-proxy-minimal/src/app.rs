use crate::connections::Web3Connections;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

static APP_USER_AGENT: &str = concat!(
    "satoshiandkin/",
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
);

/// The application
// TODO: this debug impl is way too verbose. make something smaller
// TODO: if Web3ProxyApp is always in an Arc, i think we can avoid having at least some of these internal things in arcs
pub struct Web3ProxyApp {
    /// Send requests to the best server available
    balanced_rpcs: Arc<Web3Connections>,
}

impl fmt::Debug for Web3ProxyApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("Web3ProxyApp").finish_non_exhaustive()
    }
}

impl Web3ProxyApp {
    // #[instrument(name = "try_new_Web3ProxyApp", skip_all)]
    pub async fn try_new(
        chain_id: u64,
        balanced_rpcs: Vec<String>,
    ) -> anyhow::Result<Web3ProxyApp> {
        // make a http shared client
        // TODO: how should we configure the connection pool?
        // TODO: 5 minutes is probably long enough. unlimited is a bad idea if something is wrong with the remote server
        let http_client = reqwest::ClientBuilder::new()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(60))
            .user_agent(APP_USER_AGENT)
            .build()?;

        // TODO: attach context to this error
        let balanced_rpcs =
            Web3Connections::try_new(chain_id, balanced_rpcs, Some(http_client.clone())).await?;

        Ok(Web3ProxyApp { balanced_rpcs })
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        self.balanced_rpcs.subscribe_heads().await
    }
}
