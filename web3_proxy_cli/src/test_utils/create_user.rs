use super::TestApp;
use tracing::{info, trace};
use web3_proxy::errors::Web3ProxyResult;
use web3_proxy::frontend::users::authentication::{LoginPostResponse, PostLogin};
use web3_proxy::prelude::entities::{user, user_tier};
use web3_proxy::prelude::ethers::prelude::{LocalWallet, Signer};
use web3_proxy::prelude::ethers::types::Signature;
use web3_proxy::prelude::http::StatusCode;
use web3_proxy::prelude::migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, IntoActiveModel,
    QueryFilter,
};
use web3_proxy::prelude::reqwest;

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
    let user_login_message = r
        .get(user_login_get_url)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
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
        .unwrap();
    trace!(?user_login_response);

    assert_eq!(user_login_response.status(), StatusCode::CREATED);

    let user_login_text = user_login_response.text().await.unwrap();
    trace!("user_login_text: {:#}", user_login_text);

    let user_login_response: LoginPostResponse = serde_json::from_str(&user_login_text).unwrap();
    info!(?user_login_response);

    user_login_response
}

/// TODO: use an admin user to do this instead
#[allow(unused)]
pub async fn set_user_tier(
    x: &TestApp,
    db_conn: &DatabaseConnection,
    user: user::Model,
    tier_name: &str,
) -> Web3ProxyResult<user_tier::Model> {
    let ut = user_tier::Entity::find()
        .filter(user_tier::Column::Title.like(tier_name))
        .one(db_conn)
        .await?
        .unwrap();

    let mut user = user.into_active_model();

    user.user_tier_id = sea_orm::Set(ut.id);

    user.save(db_conn).await?;

    Ok(ut)
}
