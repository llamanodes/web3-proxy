mod common;

use crate::common::TestApp;

#[ignore]
#[test_log::test(tokio::test)]
async fn test_admin_imitate_user() {
    let x = TestApp::spawn().await;

    todo!();
}

#[ignore]
#[test_log::test(tokio::test)]
async fn test_admin_grant_credits() {
    let x = TestApp::spawn().await;

    todo!();
}

#[ignore]
#[test_log::test(tokio::test)]
async fn test_admin_change_user_tier() {
    let x = TestApp::spawn().await;

    todo!();
}
