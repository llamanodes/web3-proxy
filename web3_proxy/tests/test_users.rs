mod common;

use crate::common::TestApp;
use ethers::{signers::Signer, types::Signature};
use serde::Deserialize;
use tracing::{debug, info, trace};
use ulid::Ulid;
use web3_proxy::frontend::users::authentication::PostLogin;

/// TODO: use this type in the frontend
#[derive(Debug, Deserialize)]
struct LoginPostResponse {
    pub bearer_token: Ulid,
    // pub rpc_keys: Value,
    // /// unknown data gets put here
    // #[serde(flatten, default = "HashMap::default")]
    // pub extra: HashMap<String, serde_json::Value>,
}

/// TODO: 191 and the other message formats in another test
#[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[test_log::test(tokio::test)]
async fn test_log_in_and_out() {
    let x = TestApp::spawn(true).await;

    let r = reqwest::Client::new();

    let w = x.wallet(0);

    let login_get_url = format!("{}user/login/{:?}", x.proxy_provider.url(), w.address());
    let login_message = r.get(login_get_url).send().await.unwrap();

    let login_message = login_message.text().await.unwrap();

    // sign the message and POST it
    let signed: Signature = w.sign_message(&login_message).await.unwrap();
    trace!(?signed);

    let post_login_data = PostLogin {
        msg: login_message,
        sig: signed.to_string(),
        referral_code: None,
    };
    debug!(?post_login_data);

    let login_post_url = format!("{}user/login", x.proxy_provider.url());
    let login_response = r
        .post(login_post_url)
        .json(&post_login_data)
        .send()
        .await
        .unwrap()
        .json::<LoginPostResponse>()
        .await
        .unwrap();

    info!(?login_response);

    // use the bearer token to log out
    let logout_post_url = format!("{}user/logout", x.proxy_provider.url());
    let logout_response = r
        .post(logout_post_url)
        .bearer_auth(login_response.bearer_token)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    info!(?logout_response);

    assert_eq!(logout_response, "goodbye");
}

// #[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[ignore = "under construction"]
#[test_log::test(tokio::test)]
async fn test_referral_bonus() {
    let x = TestApp::spawn(true).await;

    todo!();
}
