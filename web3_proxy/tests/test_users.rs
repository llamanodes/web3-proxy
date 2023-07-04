mod common;

use crate::common::admin_increases_balance::admin_increase_balance;
use crate::common::create_admin::create_user_as_admin;
use crate::common::create_user::create_user;
use crate::common::get_rpc_key::{user_get_first_rpc_key, RpcKey};
use crate::common::get_user_balance::user_get_balance;
use crate::common::referral::{
    get_referral_code, get_shared_referral_codes, get_used_referral_codes, UserSharedReferralInfo,
    UserUsedReferralInfo,
};
use crate::common::TestApp;
use ethers::prelude::{Http, Provider};
use ethers::{signers::Signer, types::Signature};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::time::Duration;
use tokio_stream::StreamExt;
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

#[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[test_log::test(tokio::test)]
async fn test_referral_bonus() {
    info!("Starting referral bonus test");
    let x = TestApp::spawn(true).await;
    let r = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();

    let user_wallet = x.wallet(0);
    let referrer_wallet = x.wallet(1);
    let admin_wallet = x.wallet(2);

    // Create three users, one referrer, one admin who bumps both their balances
    let referrer_login_response = create_user(&x, &r, &referrer_wallet, None).await;
    let admin_login_response = create_user_as_admin(&x, &r, &admin_wallet).await;
    // Get the first user's referral link
    let referral_link = get_referral_code(&x, &r, &referrer_login_response).await;

    let user_login_response = create_user(&x, &r, &user_wallet, Some(referral_link.clone())).await;

    // Bump both user's wallet to $20
    admin_increase_balance(
        &x,
        &r,
        &admin_login_response,
        &user_wallet,
        Decimal::from(20),
    )
    .await;
    admin_increase_balance(
        &x,
        &r,
        &admin_login_response,
        &referrer_wallet,
        Decimal::from(20),
    )
    .await;

    // Get balance before for both users
    let user_balance_response = user_get_balance(&x, &r, &user_login_response).await;
    let user_balance_pre =
        Decimal::from_str(user_balance_response["balance"].as_str().unwrap()).unwrap();
    let referrer_balance_response = user_get_balance(&x, &r, &user_login_response).await;
    let referrer_balance_pre =
        Decimal::from_str(referrer_balance_response["balance"].as_str().unwrap()).unwrap();

    // Make sure they both have balance now
    assert_eq!(user_balance_pre, Decimal::from(20));
    assert_eq!(referrer_balance_pre, Decimal::from(20));

    // Setup variables that will be used
    let shared_referral_code: UserSharedReferralInfo =
        get_shared_referral_codes(&x, &r, &referrer_login_response).await;
    let used_referral_code: UserUsedReferralInfo =
        get_used_referral_codes(&x, &r, &user_login_response).await;

    // assert that the used referral code is used
    assert_eq!(
        format!("{:?}", user_wallet.address()),
        shared_referral_code
            .clone()
            .referrals
            .get(0)
            .unwrap()
            .referred_address
            .clone()
            .unwrap()
    );
    assert_eq!(
        referral_link.clone(),
        used_referral_code
            .clone()
            .referrals
            .get(0)
            .unwrap()
            .used_referral_code
            .clone()
            .unwrap()
    );

    // We make sure that the referrer has $10 + 10% of the used balance
    // The admin provides credits for both
    let user_balance_response = user_get_balance(&x, &r, &user_login_response).await;
    let user_balance_post =
        Decimal::from_str(user_balance_response["balance"].as_str().unwrap()).unwrap();
    let referrer_balance_response = user_get_balance(&x, &r, &user_login_response).await;
    let referrer_balance_post =
        Decimal::from_str(referrer_balance_response["balance"].as_str().unwrap()).unwrap();

    // Now both users should make concurrent requests
    // TODO: Make concurrent requests
    info!("Get rpc key");
    let example_request =
        serde_json::from_str(r#"{"method":"eth_blockNumber","params":[],"id":1,"jsonrpc":"2.0"}"#)
            .unwrap();

    // Make a for-loop just spam it a bit
    // Make a JSON request
    // TODO: Also get the RPC key for the user first ...
    let rpc_keys: RpcKey = user_get_first_rpc_key(&x, &r, &user_login_response).await;
    info!("Rpc key is: {:?}", rpc_keys);
    info!(?rpc_keys);

    let proxy_endpoint = format!(
        "http://127.0.0.1:{}/rpc/{}",
        frontend_port, rpc_keys.secret_key
    );
    let proxy_provider = Provider::<Http>::try_from(proxy_endpoint).unwrap();

    let rpc_link = format!("{}rpc/{}", x.proxy_provider.url(), rpc_keys.secret_key);
    info!(?rpc_link);
    let test = r
        .get(rpc_link.clone())
        // .bearer_auth(user_login_response.bearer_token)
        .json(&example_request);
    info!("Testing rpc");
    info!(?test);
    let response = r
        .get(rpc_link)
        // .bearer_auth(user_login_response.bearer_token)
        .json(&example_request)
        .send()
        .await
        .unwrap();
    info!("Response is");
    info!(?response);
    assert_eq!(response.status(), 200);

    // let difference = user_balance_pre - user_balance_post;
    // // Finally, make sure that referrer has received 10$ of balances
    // assert_eq!(
    //     referrer_balance_pre + difference / Decimal::from(10),
    //     referrer_balance_post
    // );
}
