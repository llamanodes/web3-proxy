use ethers::providers::{Authorization, ConnectionDetails};
use std::time::Duration;
use url::Url;

// TODO: our own structs for these that handle streaming large responses
pub type EthersHttpProvider = ethers::providers::Provider<ethers::providers::Http>;
pub type EthersWsProvider = ethers::providers::Provider<ethers::providers::Ws>;

pub fn extract_auth(url: &mut Url) -> Option<Authorization> {
    if let Some(pass) = url.password().map(|x| x.to_string()) {
        // to_string is needed because we are going to remove these items from the url
        let user = url.username().to_string();

        // clear username and password from the url
        url.set_username("")
            .expect("unable to clear username on websocket");
        url.set_password(None)
            .expect("unable to clear password on websocket");

        // keep them
        Some(Authorization::basic(user, pass))
    } else {
        None
    }
}

/// Note, if the http url has an authority the http_client param is ignored and a dedicated http_client will be used
/// TODO: take a reqwest::Client or a reqwest::ClientBuilder. that way we can do things like set compression even when auth is set
pub fn connect_http(
    mut url: Url,
    http_client: Option<reqwest::Client>,
    interval: Duration,
) -> anyhow::Result<EthersHttpProvider> {
    let auth = extract_auth(&mut url);

    let mut provider = if url.scheme().starts_with("http") {
        let provider = if let Some(auth) = auth {
            ethers::providers::Http::new_with_auth(url, auth)?
        } else if let Some(http_client) = http_client {
            ethers::providers::Http::new_with_client(url, http_client)
        } else {
            ethers::providers::Http::new(url)
        };

        // TODO: i don't think this interval matters for our uses, but we should probably set it to like `block time / 2`
        ethers::providers::Provider::new(provider).interval(Duration::from_secs(2))
    } else {
        return Err(anyhow::anyhow!(
            "only http servers are supported. cannot use {}",
            url
        ));
    };

    provider.set_interval(interval);

    Ok(provider)
}

pub async fn connect_ws(mut url: Url, reconnects: usize) -> anyhow::Result<EthersWsProvider> {
    let auth = extract_auth(&mut url);

    let provider = if url.scheme().starts_with("ws") {
        let provider = if auth.is_some() {
            let connection_details = ConnectionDetails::new(url.as_str(), auth);

            // if they error, we do our own reconnection with backoff
            ethers::providers::Ws::connect_with_reconnects(connection_details, reconnects).await?
        } else {
            ethers::providers::Ws::connect_with_reconnects(url.as_str(), reconnects).await?
        };

        // TODO: dry this up (needs https://github.com/gakonst/ethers-rs/issues/592)
        // TODO: i don't think this interval matters
        ethers::providers::Provider::new(provider)
    } else {
        return Err(anyhow::anyhow!("ws servers are supported"));
    };

    Ok(provider)
}
