// So the API needs to show for any given user:
// - show balance in USD
// - show deposits history (currency, amounts, transaction id)
// - show number of requests used (so we can calculate average spending over a month, burn rate for a user etc, something like "Your balance will be depleted in xx days)
// - the email address of a user if he opted in to get contacted via email
// - all the monitoring and stats but that will come from someplace else if I understand corectly?
// I wonder how we handle payment
// probably have to do manual withdrawals

use super::{
    errors::{anyhow_error_into_response, FrontendResult},
    rate_limit::RateLimitResult,
};
use crate::app::Web3ProxyApp;
use axum::{
    extract::Path,
    response::{IntoResponse, Response},
    Extension, Json,
};
use axum_client_ip::ClientIp;
use axum_macros::debug_handler;
use entities::{user, user_keys};
use ethers::{prelude::Address, types::Bytes};
use hashbrown::HashMap;
use redis_rate_limit::redis::AsyncCommands;
use reqwest::StatusCode;
use sea_orm::ActiveModelTrait;
use serde::Deserialize;
use siwe::Message;
use std::ops::Add;
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

// TODO: how do we customize axum's error response? I think we probably want an enum that implements IntoResponse instead
#[debug_handler]
pub async fn get_login(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
    // TODO: what does axum's error handling look like if the path fails to parse?
    // TODO: allow ENS names here?
    Path(mut params): Path<HashMap<String, String>>,
) -> FrontendResult {
    // TODO: refactor this to use the try operator
    let _ip = match app.rate_limit_by_ip(ip).await {
        Ok(x) => match x.try_into_response().await {
            Ok(RateLimitResult::AllowedIp(x)) => x,
            Err(err_response) => return Ok(err_response),
            _ => unimplemented!(),
        },
        Err(err) => return Ok(anyhow_error_into_response(None, None, err)),
    };

    // at first i thought about checking that user_address is in our db
    // but theres no need to separate the create_user and login flows
    // its a better UX to just click "login with ethereum" and have the account created if it doesn't exist
    // we can prompt for an email and and payment after they log in

    // TODO: how many seconds? get from config?
    let expire_seconds: usize = 300;

    // create a message and save it in redis
    let nonce = Uuid::new_v4();

    let issued_at = OffsetDateTime::now_utc();

    let expiration_time = issued_at.add(Duration::new(expire_seconds as i64, 0));

    // TODO: proper errors. the first unwrap should be impossible, but the second will happen with bad input
    let user_address: Address = params.remove("user_address").unwrap().parse().unwrap();

    // TODO: get most of these from the app config
    let message = Message {
        domain: "staging.llamanodes.com".parse().unwrap(),
        address: user_address.to_fixed_bytes(),
        statement: Some("🦙🦙🦙🦙🦙".to_string()),
        uri: "https://staging.llamanodes.com/".parse().unwrap(),
        version: siwe::Version::V1,
        chain_id: 1,
        expiration_time: Some(expiration_time.into()),
        issued_at: issued_at.into(),
        nonce: nonce.to_string(),
        not_before: None,
        request_id: None,
        resources: vec![],
    };

    let session_key = format!("pending:{}", nonce);

    // TODO: if no redis server, store in local cache?
    let redis_pool = app
        .redis_pool
        .as_ref()
        .expect("login requires a redis server");

    let mut redis_conn = redis_pool.get().await?;

    // TODO: the address isn't enough. we need to save the actual message
    redis_conn
        .set_ex(session_key, message.to_string(), expire_seconds)
        .await?;

    // there are multiple ways to sign messages and not all wallets support them
    let message_eip = params
        .remove("message_eip")
        .unwrap_or_else(|| "eip4361".to_string());

    let message: String = match message_eip.as_str() {
        "eip4361" => message.to_string(),
        // https://github.com/spruceid/siwe/issues/98
        "eip191_string" => Bytes::from(message.eip191_string().unwrap()).to_string(),
        "eip191_hash" => Bytes::from(&message.eip191_hash().unwrap()).to_string(),
        _ => todo!("return a proper error"),
    };

    Ok(message.into_response())
}

#[debug_handler]
pub async fn create_user(
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
    Json(payload): Json<CreateUser>,
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
) -> Response {
    // TODO: return a Result instead
    let _ip = match app.rate_limit_by_ip(ip).await {
        Ok(x) => match x.try_into_response().await {
            Ok(RateLimitResult::AllowedIp(x)) => x,
            Err(err_response) => return err_response,
            _ => unimplemented!(),
        },
        Err(err) => return anyhow_error_into_response(None, None, err),
    };

    // TODO: check invite_code against the app's config or database
    if payload.invite_code != "llam4n0des!" {
        todo!("proper error message")
    }

    let redis_pool = app
        .redis_pool
        .as_ref()
        .expect("login requires a redis server");

    let mut redis_conn = redis_pool.get().await.unwrap();

    // TODO: use getdel
    // TODO: do not unwrap. make this function return a FrontendResult
    let message: String = redis_conn.get(payload.nonce.to_string()).await.unwrap();

    let message: Message = message.parse().unwrap();

    // TODO: dont unwrap. proper error
    let signature: [u8; 65] = payload.signature.as_ref().try_into().unwrap();

    // TODO: calculate the expected message for the current user. include domain and a nonce. let timestamp be automatic
    if let Err(e) = message.verify(signature, None, None, None) {
        // message cannot be correctly authenticated
        todo!("proper error message: {}", e)
    }

    let user = user::ActiveModel {
        address: sea_orm::Set(payload.address.to_fixed_bytes().into()),
        email: sea_orm::Set(payload.email),
        ..Default::default()
    };

    let db = app.db_conn.as_ref().unwrap();

    // TODO: proper error message
    let user = user.insert(db).await.unwrap();

    // TODO: create
    let api_key = todo!();

    /*
    let rpm = app.config.something;

    // create a key for the new user
    // TODO: requests_per_minute should be configurable
    let uk = user_keys::ActiveModel {
        user_id: u.id,
        api_key: sea_orm::Set(api_key),
        requests_per_minute: sea_orm::Set(rpm),
        ..Default::default()
    };

    // TODO: if this fails, rever adding the user, too
    let uk = uk.save(&txn).await.context("Failed saving new user key")?;

    // TODO: do not expose user ids
    (StatusCode::CREATED, Json(user)).into_response()
     */
}

// the input to our `create_user` handler
#[derive(Deserialize)]
pub struct CreateUser {
    address: Address,
    // TODO: make sure the email address is valid
    email: Option<String>,
    signature: Bytes,
    nonce: Uuid,
    invite_code: String,
}
