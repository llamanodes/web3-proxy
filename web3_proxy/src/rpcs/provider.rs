use anyhow::anyhow;
use derive_more::From;
use ethers::providers::{Authorization, ConnectionDetails};
use std::{borrow::Cow, time::Duration};
use url::Url;

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
    // TODO: deadpool? custom tokio-tungstenite
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

    /// Note, if the http url has an authority the http_client param is ignored and a dedicated http_client will be used
    /// TODO: take a reqwest::Client or a reqwest::ClientBuilder. that way we can do things like set compression even when auth is set
    pub async fn new(
        mut url: Cow<'_, Url>,
        http_client: Option<reqwest::Client>,
    ) -> anyhow::Result<Self> {
        let auth = if let Some(pass) = url.password().map(|x| x.to_string()) {
            // to_string is needed because we are going to remove these items from the url
            let user = url.username().to_string();

            // clear username and password from the url
            let mut_url = url.to_mut();

            mut_url
                .set_username("")
                .map_err(|_| anyhow!("unable to clear username on websocket"))?;
            mut_url
                .set_password(None)
                .map_err(|_| anyhow!("unable to clear password on websocket"))?;

            // keep them
            Some(Authorization::basic(user, pass))
        } else {
            None
        };

        let provider = if url.scheme().starts_with("http") {
            let provider = if let Some(auth) = auth {
                ethers::providers::Http::new_with_auth(url.into_owned(), auth)?
            } else if let Some(http_client) = http_client {
                ethers::providers::Http::new_with_client(url.into_owned(), http_client)
            } else {
                ethers::providers::Http::new(url.into_owned())
            };

            // TODO: i don't think this interval matters for our uses, but we should probably set it to like `block time / 2`
            ethers::providers::Provider::new(provider)
                .interval(Duration::from_secs(12))
                .into()
        } else if url.scheme().starts_with("ws") {
            let provider = if auth.is_some() {
                let connection_details = ConnectionDetails::new(url.as_str(), auth);

                ethers::providers::Ws::connect(connection_details).await?
            } else {
                ethers::providers::Ws::connect(url.as_str()).await?
            };

            // TODO: dry this up (needs https://github.com/gakonst/ethers-rs/issues/592)
            // TODO: i don't think this interval matters
            ethers::providers::Provider::new(provider).into()
        } else {
            return Err(anyhow::anyhow!("only http and ws servers are supported"));
        };

        Ok(provider)
    }
}
