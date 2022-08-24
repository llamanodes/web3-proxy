use std::time::Duration;

use derive_more::From;
use tracing::{info_span, instrument, Instrument};

/// Use HTTP and WS providers.
// TODO: instead of an enum, I tried to use Box<dyn Provider>, but hit <https://github.com/gakonst/ethers-rs/issues/592>
#[derive(From)]
pub enum Web3Provider {
    Http(ethers::providers::Provider<ethers::providers::Http>),
    Ws(ethers::providers::Provider<ethers::providers::Ws>),
}

impl Web3Provider {
    #[instrument]
    pub async fn from_str(
        url_str: &str,
        http_client: Option<reqwest::Client>,
    ) -> anyhow::Result<Self> {
        let provider = if url_str.starts_with("http") {
            let url: url::Url = url_str.parse()?;

            let http_client = http_client.ok_or_else(|| anyhow::anyhow!("no http_client"))?;

            let provider = ethers::providers::Http::new_with_client(url, http_client);

            // TODO: dry this up (needs https://github.com/gakonst/ethers-rs/issues/592)
            // TODO: i don't think this interval matters for our uses, but we should probably set it to like `block time / 2`
            ethers::providers::Provider::new(provider)
                .interval(Duration::from_secs(13))
                .into()
        } else if url_str.starts_with("ws") {
            let provider = ethers::providers::Ws::connect(url_str)
                .instrument(info_span!("Web3Provider", %url_str))
                .await?;

            // TODO: dry this up (needs https://github.com/gakonst/ethers-rs/issues/592)
            // TODO: i don't think this interval matters
            ethers::providers::Provider::new(provider).into()
        } else {
            return Err(anyhow::anyhow!("only http and ws servers are supported"));
        };

        Ok(provider)
    }
}
