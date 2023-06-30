mod common;

use crate::common::TestApp;
use ethers::signers::Signer;
use tracing::info;

/// TODO: 191 and the other message formats in another test
#[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[test_log::test(tokio::test)]
async fn test_log_in_and_out() {
    let x = TestApp::spawn(true).await;

    let w = x.wallet(0);

    let login_url = format!("{}user/login/{:?}", x.proxy_provider.url(), w.address());
    let login_response = reqwest::get(login_url).await.unwrap();

    info!(?login_response);

    // TODO: sign the message and POST it

    // TODO: get bearer token out of response

    // TODO: log out

    todo!();
}

#[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[ignore = "under construction"]
#[test_log::test(tokio::test)]
async fn test_referral_bonus() {
    let x = TestApp::spawn(true).await;

    todo!();
}
