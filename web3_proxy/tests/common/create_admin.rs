use crate::TestApp;
use ethers::prelude::{LocalWallet, Signer};
use ethers::types::Signature;
use tracing::info;
use web3_proxy::frontend::users::authentication::{LoginPostResponse, PostLogin};
use web3_proxy::sub_commands::ChangeAdminStatusSubCommand;

/// Helper function to create admin

/// Create user as admin
#[allow(unused)]
pub async fn create_user_as_admin(
    x: &TestApp,
    r: &reqwest::Client,
    admin_wallet: &LocalWallet,
) -> LoginPostResponse {
    // Create the account
    let login_post_url = format!("{}user/login", x.proxy_provider.url());
    let admin_login_get_url = format!(
        "{}user/login/{:?}",
        x.proxy_provider.url(),
        admin_wallet.address()
    );
    let admin_login_message = r.get(admin_login_get_url).send().await.unwrap();
    let admin_login_message = admin_login_message.text().await.unwrap();

    // Sign the message and POST it to login as admin
    let admin_signed: Signature = admin_wallet
        .sign_message(&admin_login_message)
        .await
        .unwrap();
    info!(?admin_signed);

    let admin_post_login_data = PostLogin {
        msg: admin_login_message,
        sig: admin_signed.to_string(),
        referral_code: None,
    };
    info!(?admin_post_login_data);

    let admin_login_response = r
        .post(&login_post_url)
        .json(&admin_post_login_data)
        .send()
        .await
        .unwrap()
        .json::<LoginPostResponse>()
        .await
        .unwrap();
    info!(?admin_login_response);

    // Upgrade the account to admin
    info!("Make the user an admin ...");
    // Change Admin SubCommand struct
    let admin_status_changer = ChangeAdminStatusSubCommand {
        address: format!("{:?}", admin_wallet.address()),
        should_be_admin: true,
    };
    info!(?admin_status_changer);

    info!("Changing the status of the admin_wallet to be an admin");
    // Pass on the database into it ...
    admin_status_changer.main(x.db_conn()).await.unwrap();

    // Now log him in again, because he was just signed out
    // Login the admin again, because he was just signed out
    let admin_login_get_url = format!(
        "{}user/login/{:?}",
        x.proxy_provider.url(),
        admin_wallet.address()
    );
    let admin_login_message = r.get(admin_login_get_url).send().await.unwrap();
    let admin_login_message = admin_login_message.text().await.unwrap();

    // Sign the message and POST it to login as admin
    let admin_signed: Signature = admin_wallet
        .sign_message(&admin_login_message)
        .await
        .unwrap();
    info!(?admin_signed);

    let admin_post_login_data = PostLogin {
        msg: admin_login_message,
        sig: admin_signed.to_string(),
        referral_code: None,
    };
    info!(?admin_post_login_data);

    let admin_login_response = r
        .post(&login_post_url)
        .json(&admin_post_login_data)
        .send()
        .await
        .unwrap()
        .json::<LoginPostResponse>()
        .await
        .unwrap();
    info!(?admin_login_response);

    admin_login_response
}
