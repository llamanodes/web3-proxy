use crate::rpcs::provider::EthersHttpProvider;
use ulid::Ulid;
use url::Url;

pub async fn create_provider_for_user(url: &Url, user_secret_key: &Ulid) -> EthersHttpProvider {
    // Then generate a provider
    let proxy_endpoint = format!("{}rpc/{}", url, user_secret_key);

    EthersHttpProvider::try_from(proxy_endpoint).unwrap()
}
