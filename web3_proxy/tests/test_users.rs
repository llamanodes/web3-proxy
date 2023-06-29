mod common;

use crate::common::TestApp;

#[ignore]
#[test_log::test(tokio::test)]
async fn test_log_in_and_out() {
    let x = TestApp::spawn().await;

    todo!();
}

#[ignore]
#[test_log::test(tokio::test)]
async fn test_referral_bonus() {
    let x = TestApp::spawn().await;

    todo!();
}
