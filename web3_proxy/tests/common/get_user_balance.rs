use crate::TestApp;
use ethers::prelude::{LocalWallet, Signer};
use ethers::types::Signature;
use reqwest::Response;
use rust_decimal::Decimal;
use tracing::info;
use web3_proxy::frontend::admin::AdminIncreaseBalancePost;
use web3_proxy::frontend::users::authentication::{LoginPostResponse, PostLogin};

/// Helper function to get the user's balance
pub async fn user_get_balance(
    x: &TestApp,
    r: &reqwest::Client,
    login_response: &LoginPostResponse,
) -> (serde_json::Value) {
    let get_user_balance = format!("{}user/balance", x.proxy_provider.url());
    info!("Get balance");
    let balance_response = r
        .get(get_user_balance)
        .bearer_auth(login_response.bearer_token)
        .send()
        .await
        .unwrap();
    info!(?balance_response);

    let balance_response = balance_response.json::<serde_json::Value>().await.unwrap();
    info!(?balance_response);

    balance_response
}
