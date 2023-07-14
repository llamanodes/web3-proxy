use ulid::Ulid;
use url::Url;
use web3_proxy::rpcs::provider::EthersHttpProvider;

#[allow(unused)]
pub async fn create_provider_for_user(url: &Url, user_secret_key: &Ulid) -> EthersHttpProvider {
    // Then generate a provider
    let proxy_endpoint = format!("{}rpc/{}", url, user_secret_key);

    EthersHttpProvider::try_from(proxy_endpoint).unwrap()
}
