mod common;

use crate::common::TestApp;
use ethers::{signers::Signer, types::Signature};
use migration::Value::Decimal;
use serde::Deserialize;
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

// #[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[ignore = "under construction"]
#[test_log::test(tokio::test)]
async fn test_referral_bonus() {
    info!("Starting referral bonus test");
    let x = TestApp::spawn(true).await;
    let r = reqwest::Client::new()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();

    // Setup variables that will be used
    let login_post_url = format!("{}user/login", x.proxy_provider.url());
    let get_referral_link = format!("{}user/referral", x.proxy_provider.url());
    let get_user_balance = format!("{}user/balance", x.proxy_provider.url());
    // let admin_provide_balance = format!("");

    let check_used_referral_link =
        format!("{}/user/referral/stats/used-codes", x.proxy_provider.url());
    let check_shared_referral_link = format!(
        "{}/user/referral/stats/shared-codes",
        x.proxy_provider.url()
    );

    let referrer_wallet = x.wallet(1);
    let user_wallet = x.wallet(2);

    // Login the referrer
    let referrer_login_get_url = format!(
        "{}user/login/{:?}",
        x.proxy_provider.url(),
        referrer_wallet.address()
    );
    let referrer_login_message = r.get(referrer_login_get_url).send().await.unwrap();
    let referrer_login_message = referrer_login_message.text().await.unwrap();
    // Sign the message and POST it to login as admin
    let referrer_signed: Signature = referrer_wallet
        .sign_message(&referrer_login_message)
        .await
        .unwrap();
    info!(?referrer_signed);
    let referrer_post_login_data = PostLogin {
        msg: referrer_login_message,
        sig: referrer_signed.to_string(),
        referral_code: None,
    };
    info!(?referrer_post_login_data);
    let referrer_login_response = r
        .post(&login_post_url)
        .json(&referrer_post_login_data)
        .send()
        .await
        .unwrap()
        .json::<LoginPostResponse>()
        .await
        .unwrap();
    info!(?referrer_login_response);

    // Now the referrer needs to access his referral code
    let referrer_get_referral_link_login = r
        .post(&get_referral_link)
        .send()
        .await
        .unwrap()
        .json::<GetReferralLinkResponse>()
        .await
        .unwrap();
    info!(?referrer_get_referral_link_login);

    // Also login the user (to create the user)
    let user_login_get_url = format!(
        "{}user/login/{:?}",
        x.proxy_provider.url(),
        user_wallet.address()
    );
    let user_login_message = r.get(user_login_get_url).send().await.unwrap();
    let user_login_message = user_login_message.text().await.unwrap();

    // Sign the message and POST it to login as admin
    let user_signed: Signature = user_wallet.sign_message(&user_login_message).await.unwrap();
    info!(?user_signed);
    let user_post_login_data = PostLogin {
        msg: user_login_message,
        sig: user_signed.to_string(),
        referral_code: Some(referrer_get_referral_link_login.referral_code),
    };
    info!(?user_post_login_data);
    let user_login_response = r
        .post(&login_post_url)
        .json(&user_post_login_data)
        .send()
        .await
        .unwrap()
        .json::<LoginPostResponse>()
        .await
        .unwrap();
    info!(?user_login_response);

    // The referrer makes sure that the user is registered as a referred used
    let referrer_check_user = r
        .post(&check_shared_referral_link)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    info!(?"Referrer check user");
    info!(?referrer_check_user);

    // The referee makes sure that the referrer is registered as the referrer
    let user_check_referrer = r
        .post(&check_used_referral_link)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    info!(?"User check referrer");
    info!(?user_check_referrer);

    // TODO: Assign credits to both users (10$)

    // Then we ask both users for their balance
    let referrer_balance_pre = r
        .post(get_user_balance.clone())
        .bearer_auth(referrer_login_response.bearer_token)
        .send()
        .await
        .unwrap();
    info!(?referrer_balance_pre);
    let user_balance_pre = r
        .post(get_user_balance.clone())
        .bearer_auth(user_login_response.bearer_token)
        .send()
        .await
        .unwrap();
    info!(?user_balance_pre);

    // We make sure that the referrer has $10 + 10% of the used balance
    // The admin provides credits for both

    assert_eq!(referrer_balance_pre, Decimal::from(20));
    assert_eq!(user_balance_pre, Decimal::from(20));

    // Now both users should make concurrent requests

    // Then we assert the balances to be distributed
    let referrer_balance_post = r
        .post(get_user_balance.clone())
        .bearer_auth(referrer_login_response.bearer_token)
        .send()
        .await
        .unwrap();
    info!(?referrer_balance_post);
    let user_balance_post = r
        .post(get_user_balance.clone())
        .bearer_auth(user_login_response.bearer_token)
        .send()
        .await
        .unwrap();
    info!(?user_balance_post);

    let difference = user_balance_pre - user_balance_post;
    // Finally, make sure that referrer has received 10$ of balances
    assert_eq!(
        referrer_balance_pre + difference / Decimal::from(10),
        referrer_balance_post
    );
}
