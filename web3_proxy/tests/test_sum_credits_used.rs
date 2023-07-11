mod common;
use web3_proxy::balance::Balance;

use crate::common::{
    admin_increases_balance::admin_increase_balance, create_admin::create_user_as_admin,
    create_user::create_user, get_user_balance::user_get_balance, TestApp,
};
use migration::sea_orm::prelude::Decimal;
use std::time::Duration;

#[test_log::test(tokio::test)]
async fn test_sum_credits_used() {
    let x = TestApp::spawn(999_001_999, true).await;

    let r = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();

    // create wallets for users
    let user_wallet = x.wallet(0);
    let admin_wallet = x.wallet(1);

    // log in to create users
    let admin_login_response = create_user_as_admin(&x, &r, &admin_wallet).await;
    let user_login_response = create_user(&x, &r, &user_wallet, None).await;

    // Bump user wallet to $1000
    admin_increase_balance(
        &x,
        &r,
        &admin_login_response,
        &user_wallet,
        Decimal::from(1000),
    )
    .await;

    // check balance
    let balance_url = format!("{}user/balance", x.proxy_provider.url());

    let balance: Balance = user_get_balance(&x, &r, &user_login_response).await;

    // make one rpc request of 10 CU

    // flush stats

    // check balance

    // make one rpc request of 10 CU

    // flush stats

    // check balance

    // make ten rpc request of 10 CU

    // flush stats

    // check balance

    todo!();
}
