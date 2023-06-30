mod common;

use crate::common::TestApp;

// #[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[ignore = "under construction"]
#[test_log::test(tokio::test)]
async fn test_admin_imitate_user() {
    let x = TestApp::spawn(true).await;

    todo!();
}

// #[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[ignore = "under construction"]
#[test_log::test(tokio::test)]
async fn test_admin_grant_credits() {
    let x = TestApp::spawn(true).await;

    todo!();
}

// #[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
#[ignore = "under construction"]
#[test_log::test(tokio::test)]
async fn test_admin_change_user_tier() {
    let x = TestApp::spawn(true).await;

    todo!();
}
