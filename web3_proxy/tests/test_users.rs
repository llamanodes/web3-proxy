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
use tokio::time;
use tokio::time::sleep;
use tokio_stream::StreamExt;
use tracing::{debug, info, trace};
use ulid::Ulid;
use web3_proxy::frontend::users::authentication::PostLogin;
use web3_proxy::rpcs::blockchain::ArcBlock;

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
async fn test_user_balance_decreases() {
    info!("Starting balance decreases with usage test");
    let x = TestApp::spawn(true).await;
    let r = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap();

    let user_wallet = x.wallet(0);
    let admin_wallet = x.wallet(1);

    // Create three users, one referrer, one admin who bumps both their balances
    let admin_login_response = create_user_as_admin(&x, &r, &admin_wallet).await;
    let user_login_response = create_user(&x, &r, &user_wallet, None).await;

    // Get the rpc keys for this user
    let rpc_keys: RpcKey = user_get_first_rpc_key(&x, &r, &user_login_response).await;
    let proxy_endpoint = format!("{}rpc/{}", x.proxy_provider.url(), rpc_keys.secret_key);
    let proxy_provider = Provider::<Http>::try_from(proxy_endpoint).unwrap();

    // Make somre requests while in the free tier, so we can test bookkeeping here
    for _ in 1..10_000 {
        let proxy_result = proxy_provider
            .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
            .await
            .unwrap()
            .unwrap();
    }

    // Flush all stats here
    x.flush_stats().await.unwrap();
    let now = time::Instant::now();
    sleep(Duration::from_secs(2)).await;
    let later = time::Instant::now();
    assert!(later - now >= Duration::from_secs(2));
    info!("Now vs later {:?} {:?}", now, later);

    // Bump both user's wallet to $20
    admin_increase_balance(
        &x,
        &r,
        &admin_login_response,
        &user_wallet,
        Decimal::from(20),
    )
    .await;
    let user_balance_response = user_get_balance(&x, &r, &user_login_response).await;
    let user_balance_pre =
        Decimal::from_str(user_balance_response["balance"].as_str().unwrap()).unwrap();
    assert_eq!(user_balance_pre, Decimal::from(20));

    for _ in 1..10_000 {
        let proxy_result = proxy_provider
            .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
            .await
            .unwrap()
            .unwrap();
    }

    // Flush all stats here
    x.flush_stats().await.unwrap();
    let now = time::Instant::now();
    sleep(Duration::from_secs(2)).await;
    let later = time::Instant::now();
    assert!(later - now >= Duration::from_secs(2));
    info!("Now vs later {:?} {:?}", now, later);

    // Deposits should not be affected, and should be equal to what was initially provided
    let total_deposits =
        Decimal::from_str(user_balance_response["total_deposits"].as_str().unwrap()).unwrap();
    assert_eq!(total_deposits, Decimal::from(20));

    // Get the full balance endpoint
    let user_balance_response = user_get_balance(&x, &r, &user_login_response).await;
    let user_balance_post =
        Decimal::from_str(user_balance_response["balance"].as_str().unwrap()).unwrap();
    assert!(user_balance_post < user_balance_pre);

    // Balance should be total deposits - usage while in the paid tier
    let total_spent_outside_free_tier = Decimal::from_str(
        user_balance_response["total_spent_outside_free_tier"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        total_deposits - total_spent_outside_free_tier,
        user_balance_post
    );

    // This should never be negative
    let user_balance_total_spent =
        Decimal::from_str(user_balance_response["total_spent"].as_str().unwrap()).unwrap();
    assert!(user_balance_total_spent > Decimal::from(0));
}

#[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[test_log::test(tokio::test)]
async fn test_referral_bonus_non_concurrent() {
    info!("Starting referral bonus test");
    let x = TestApp::spawn(true).await;
    let r = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
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
    let referrer_balance_response = user_get_balance(&x, &r, &referrer_login_response).await;
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

    // Make a for-loop just spam it a bit
    // Make a JSON request
    let rpc_keys: RpcKey = user_get_first_rpc_key(&x, &r, &user_login_response).await;
    info!("Rpc key is: {:?}", rpc_keys);

    let proxy_endpoint = format!("{}rpc/{}", x.proxy_provider.url(), rpc_keys.secret_key);
    let proxy_provider = Provider::<Http>::try_from(proxy_endpoint).unwrap();

    for _ in 1..20_000 {
        let proxy_result = proxy_provider
            .request::<_, Option<ArcBlock>>("eth_getBlockByNumber", ("latest", false))
            .await
            .unwrap()
            .unwrap();
    }

    // Flush all stats here
    x.flush_stats().await.unwrap();
    let now = time::Instant::now();
    sleep(Duration::from_secs(2)).await;
    let later = time::Instant::now();
    assert!(later - now >= Duration::from_secs(2));
    info!("Now vs later {:?} {:?}", now, later);

    // Check that at least something was earned:
    let shared_referral_code: UserSharedReferralInfo =
        get_shared_referral_codes(&x, &r, &referrer_login_response).await;
    info!("Referral code");
    info!("{:?}", shared_referral_code.referrals.get(0).unwrap());

    // We make sure that the referrer has $10 + 10% of the used balance
    // The admin provides credits for both
    let user_balance_response = user_get_balance(&x, &r, &user_login_response).await;
    let user_balance_post =
        Decimal::from_str(user_balance_response["balance"].as_str().unwrap()).unwrap();
    let referrer_balance_response = user_get_balance(&x, &r, &referrer_login_response).await;
    let referrer_balance_post =
        Decimal::from_str(referrer_balance_response["balance"].as_str().unwrap()).unwrap();

    let difference = user_balance_pre - user_balance_post;

    // Make sure that the pre and post balance is not the same (i.e. some change has occurred)
    assert_ne!(
        user_balance_pre, user_balance_post,
        "Pre and post balnace is equivalent"
    );
    assert!(user_balance_pre > user_balance_post);
    assert!(referrer_balance_pre < referrer_balance_post);

    // Finally, make sure that referrer has received 10$ of balances
    assert_eq!(
        referrer_balance_pre + difference / Decimal::from(10),
        referrer_balance_post
    );
}
