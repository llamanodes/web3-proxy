use crate::TestApp;
use ethers::prelude::{LocalWallet, Signer};
use ethers::types::Signature;
use tracing::info;
use web3_proxy::frontend::users::authentication::{LoginPostResponse, PostLogin};

/// Helper function to create an "ordinary" user
#[allow(unused)]
pub async fn create_user(
    x: &TestApp,
    r: &reqwest::Client,
    user_wallet: &LocalWallet,
    referral_code: Option<String>,
) -> (LoginPostResponse) {
    let login_post_url = format!("{}user/login", x.proxy_provider.url());
    let user_login_get_url = format!(
        "{}user/login/{:?}",
        x.proxy_provider.url(),
        user_wallet.address()
    );
    let user_login_message = r.get(user_login_get_url).send().await.unwrap();
    let user_login_message = user_login_message.text().await.unwrap();

    // Sign the message and POST it to login as the user
    let user_signed: Signature = user_wallet.sign_message(&user_login_message).await.unwrap();
    info!(?user_signed);

    let user_post_login_data = PostLogin {
        msg: user_login_message,
        sig: user_signed.to_string(),
        referral_code,
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

    user_login_response
}
