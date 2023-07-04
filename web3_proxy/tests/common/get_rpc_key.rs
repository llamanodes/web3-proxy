use crate::TestApp;
use ethers::prelude::{LocalWallet, Signer};
use ethers::types::Signature;
use reqwest::Response;
use rust_decimal::Decimal;
use serde::Deserialize;
use tracing::info;
use web3_proxy::frontend::admin::AdminIncreaseBalancePost;
use web3_proxy::frontend::users::authentication::{LoginPostResponse, PostLogin};

#[derive(Debug, Deserialize)]
pub struct RpcKeyResponse {
    pub user_id: u64,
    pub user_rpc_keys: std::collections::HashMap<String, RpcKey>,
}

#[derive(Debug, Deserialize)]
pub struct RpcKey {
    pub active: bool,
    pub allowed_ips: Option<serde_json::Value>,
    pub allowed_origins: Option<serde_json::Value>,
    pub allowed_referers: Option<serde_json::Value>,
    pub allowed_user_agents: Option<serde_json::Value>,
    pub description: Option<serde_json::Value>,
    pub id: u64,
    pub log_revert_chance: f64,
    pub private_txs: bool,
    pub role: String,
    pub secret_key: String,
    pub user_id: u64,
}

/// Helper function to get the user's balance
pub async fn user_get_first_rpc_key(
    x: &TestApp,
    r: &reqwest::Client,
    login_response: &LoginPostResponse,
) -> (RpcKey) {
    let get_keys = format!("{}user/keys", x.proxy_provider.url());

    info!("Get balance");
    let rpc_key_response = r
        .get(get_keys)
        .bearer_auth(login_response.bearer_token)
        .send()
        .await
        .unwrap();
    info!(?rpc_key_response);

    let rpc_key_response = rpc_key_response.json::<serde_json::Value>().await.unwrap();
    info!(?rpc_key_response);

    info!("Rpc Key");
    let rpc_key: RpcKeyResponse = serde_json::from_value(rpc_key_response).unwrap();
    info!(?rpc_key);

    rpc_key.user_rpc_keys.into_iter().next().unwrap().1
}
