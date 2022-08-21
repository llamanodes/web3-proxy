// So the API needs to show for any given user:
// - show balance in USD
// - show deposits history (currency, amounts, transaction id)
// - show number of requests used (so we can calculate average spending over a month, burn rate for a user etc, something like "Your balance will be depleted in xx days)
// - the email address of a user if he opted in to get contacted via email
// - all the monitoring and stats but that will come from someplace else if I understand corectly?
// I wonder how we handle payment
// probably have to do manual withdrawals

use super::rate_limit::rate_limit_by_ip;
use super::{
    errors::{anyhow_error_into_response, FrontendResult},
    rate_limit::RateLimitResult,
};
use crate::app::Web3ProxyApp;
use axum::{
    extract::{Path, Query},
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
use std::sync::Arc;
use std::{net::IpAddr, ops::Add};
use time::{Duration, OffsetDateTime};
use ulid::Ulid;

#[allow(unused)]
use super::axum_ext::empty_string_as_none;

// TODO: how do we customize axum's error response? I think we probably want an enum that implements IntoResponse instead
#[debug_handler]
pub async fn get_login(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
    // TODO: what does axum's error handling look like if the path fails to parse?
    // TODO: allow ENS names here?
    Path(mut params): Path<HashMap<String, String>>,
) -> FrontendResult {
    let _ip: IpAddr = rate_limit_by_ip(&app, ip).await?.try_into()?;

    // at first i thought about checking that user_address is in our db
    // but theres no need to separate the registration and login flows
    // its a better UX to just click "login with ethereum" and have the account created if it doesn't exist
    // we can prompt for an email and and payment after they log in

    // create a message and save it in redis

    // TODO: how many seconds? get from config?
    let expire_seconds: usize = 300;

    let nonce = Ulid::new();

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
    let mut redis_conn = app
        .redis_pool
        .as_ref()
        .expect("login requires a redis server")
        .get()
        .await?;

    // the address isn't enough. we need to save the actual message so we can read the nonce
    // TODO: what message format is the most efficient to store in redis? probably eip191_string
    redis_conn
        .set_ex(session_key, message.to_string(), expire_seconds)
        .await?;

    drop(redis_conn);

    // there are multiple ways to sign messages and not all wallets support them
    let message_eip = params
        .remove("message_eip")
        .unwrap_or_else(|| "eip4361".to_string());

    let message: String = match message_eip.as_str() {
        "eip4361" => message.to_string(),
        // https://github.com/spruceid/siwe/issues/98
        "eip191_string" => Bytes::from(message.eip191_string().unwrap()).to_string(),
        "eip191_hash" => Bytes::from(&message.eip191_hash().unwrap()).to_string(),
        _ => return Err(anyhow::anyhow!("invalid message eip given").into()),
    };

    Ok(message.into_response())
}

/// Query params to our `post_login` handler.
#[derive(Debug, Deserialize)]
pub struct PostLoginQuery {
    invite_code: Option<String>,
}

/// JSON body to our `post_login` handler.
#[derive(Deserialize)]
pub struct PostLogin {
    address: Address,
    msg: String,
    sig: Bytes,
    // TODO: do we care about these? we should probably check the version is something we expect
    // version: String,
    // signer: String,
}

#[debug_handler]
/// Post to the user endpoint to register or login.
pub async fn post_login(
    ClientIp(ip): ClientIp,
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    Json(payload): Json<PostLogin>,
    Query(query): Query<PostLoginQuery>,
) -> FrontendResult {
    let _ip: IpAddr = rate_limit_by_ip(&app, ip).await?.try_into()?;

    let mut new_user = true; // TODO: check the database

    if let Some(invite_code) = &app.config.invite_code {
        // we don't do per-user referral codes because we shouldn't collect what we don't need.
        // we don't need to build a social graph between addresses like that.
        if query.invite_code.as_ref() != Some(invite_code) {
            todo!("if address is already registered, allow login! else, error")
        }
    }

    // we can't trust that they didn't tamper with the message in some way
    let their_msg: siwe::Message = payload.msg.parse().unwrap();

    let their_sig: [u8; 65] = payload.sig.as_ref().try_into().unwrap();

    // fetch the message we gave them from our redis
    let redis_pool = app
        .redis_pool
        .as_ref()
        .expect("login requires a redis server");

    let mut redis_conn = redis_pool.get().await.unwrap();

    // TODO: use getdel
    // TODO: do not unwrap. make this function return a FrontendResult
    let our_msg: String = redis_conn.get(&their_msg.nonce).await.unwrap();

    let our_msg: siwe::Message = our_msg.parse().unwrap();

    // check the domain and a nonce. let timestamp be automatic
    if let Err(e) = their_msg.verify(their_sig, Some(&our_msg.domain), Some(&our_msg.nonce), None) {
        // message cannot be correctly authenticated
        todo!("proper error message: {}", e)
    }

    if new_user {
        // the only thing we need from them is an address
        // everything else is optional
        let user = user::ActiveModel {
            address: sea_orm::Set(payload.address.to_fixed_bytes().into()),
            ..Default::default()
        };

        let db = app.db_conn.as_ref().unwrap();

        let user = user.insert(db).await.unwrap();

        let api_key = todo!("create an api key");

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

        // TODO: set a cookie?

        // TODO: do not expose user ids
        (StatusCode::CREATED, Json(user)).into_response()
        */
    } else {
        todo!("load existing user from the database");
    }
}

/// the JSON input to the `post_user` handler
#[derive(Deserialize)]
pub struct PostUser {
    address: Address,
    // TODO: make sure the email address is valid. probably have a "verified" column in the database
    email: Option<String>,
    // TODO: make them sign this JSON? cookie in session id is hard because its on a different domain
}

#[debug_handler]
/// post to the user endpoint to modify your account
pub async fn post_user(
    Json(payload): Json<PostUser>,
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
) -> FrontendResult {
    todo!("finish post_login");

    // let user = user::ActiveModel {
    //     address: sea_orm::Set(payload.address.to_fixed_bytes().into()),
    //     email: sea_orm::Set(payload.email),
    //     ..Default::default()
    // };
}
