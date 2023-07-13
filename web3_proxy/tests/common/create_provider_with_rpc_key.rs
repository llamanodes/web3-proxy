use crate::common::rpc_key::{user_get_first_rpc_key, RpcKey};
use crate::common::TestApp;
use ethers::prelude::{Http, Provider};
use ulid::Ulid;
use url::Url;
use web3_proxy::frontend::users::authentication::LoginPostResponse;

/// Helper function to increase the balance of a user, from an admin
#[allow(unused)]
pub async fn create_provider_for_user(url: Url, user_secret_key: Ulid) -> Provider<Http> {
    // Then generate a provider
    let proxy_endpoint = format!("{}rpc/{}", url, user_secret_key);
    let proxy_provider = Provider::<Http>::try_from(proxy_endpoint).unwrap();

    proxy_provider
}
