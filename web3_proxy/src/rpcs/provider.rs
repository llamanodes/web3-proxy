use anyhow::Context;
use derive_more::From;
use std::time::Duration;

// TODO: our own structs for these that handle streaming large responses
type EthersHttpProvider = ethers::providers::Provider<ethers::providers::Http>;
type EthersWsProvider = ethers::providers::Provider<ethers::providers::Ws>;

/// Use HTTP and WS providers.
// TODO: instead of an enum, I tried to use Box<dyn Provider>, but hit <https://github.com/gakonst/ethers-rs/issues/592>
// TODO: custom types that let us stream JSON responses
#[derive(From)]
pub enum Web3Provider {
    Both(EthersHttpProvider, EthersWsProvider),
    Http(EthersHttpProvider),
    // TODO: deadpool? custom tokio-tungstenite?
    Ws(EthersWsProvider),
    #[cfg(test)]
    Mock,
}

impl Web3Provider {
    pub fn http(&self) -> Option<&EthersHttpProvider> {
        match self {
            Self::Http(x) => Some(x),
            _ => None,
        }
    }

    pub fn ws(&self) -> Option<&EthersWsProvider> {
        match self {
            Self::Both(_, x) | Self::Ws(x) => Some(x),
            _ => None,
        }
    }

    pub async fn from_str(
        url_str: &str,
        http_client: Option<reqwest::Client>,
    ) -> anyhow::Result<Self> {
        let provider = if url_str.starts_with("http") {
            let url: url::Url = url_str.parse()?;

            let http_client = http_client.context("no http_client")?;

            let provider = ethers::providers::Http::new_with_client(url, http_client);

            // TODO: dry this up (needs https://github.com/gakonst/ethers-rs/issues/592)
            // TODO: i don't think this interval matters for our uses, but we should probably set it to like `block time / 2`
            ethers::providers::Provider::new(provider)
                .interval(Duration::from_secs(12))
                .into()
        } else if url_str.starts_with("ws") {
            let provider = ethers::providers::Ws::connect(url_str).await?;

            // TODO: dry this up (needs https://github.com/gakonst/ethers-rs/issues/592)
            // TODO: i don't think this interval matters
            ethers::providers::Provider::new(provider).into()
        } else {
            return Err(anyhow::anyhow!("only http and ws servers are supported"));
        };

        Ok(provider)
    }
}
