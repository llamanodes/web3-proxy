use crate::TestApp;
use tracing::info;
use web3_proxy::balance::Balance;
use web3_proxy::frontend::users::authentication::LoginPostResponse;

/// Helper function to get the user's balance
#[allow(unused)]
pub async fn user_get_balance(
    x: &TestApp,
    r: &reqwest::Client,
    login_response: &LoginPostResponse,
) -> Balance {
    let get_user_balance = format!("{}user/balance", x.proxy_provider.url());
    info!("Get balance");
    let balance_response = r
        .get(get_user_balance)
        .bearer_auth(login_response.bearer_token)
        .send()
        .await
        .unwrap();
    info!(?balance_response);

    let balance_response = balance_response.json().await.unwrap();
    info!(?balance_response);

    balance_response
}

pub async fn user_get_total_frontend_requests(
    x: &TestApp,
    r: &reqwest::Client,
    login_response: &LoginPostResponse,
) -> u64 {
    todo!();
}
