//! Handle registration, logins, and managing account data.

use super::authorization::{login_is_authorized, UserKey};
use super::errors::FrontendResult;
use crate::app::Web3ProxyApp;
use crate::frontend::authorization::bearer_is_authorized;
use crate::user_queries::{get_aggregate_rpc_stats_from_params, get_detailed_stats};
use anyhow::Context;
use axum::{
    extract::{Path, Query},
    headers::{authorization::Bearer, Authorization},
    response::IntoResponse,
    Extension, Json, TypedHeader,
};
use axum_client_ip::ClientIp;
use axum_macros::debug_handler;
use entities::{user, user_keys};
use ethers::{prelude::Address, types::Bytes};
use hashbrown::HashMap;
use http::StatusCode;
use redis_rate_limiter::redis::AsyncCommands;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, TransactionTrait};
use serde::{Deserialize, Serialize};
use siwe::{Message, VerificationOpts};
use std::ops::Add;
use std::str::FromStr;
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
use tracing::{info, warn};
use ulid::Ulid;

/// `GET /user/login/:user_address` or `GET /user/login/:user_address/:message_eip` -- Start the "Sign In with Ethereum" (siwe) login flow.
///
/// `message_eip`s accepted:
///   - eip191_bytes
///   - eip191_hash
///   - eip4361 (default)
///
/// Coming soon: eip1271
///
/// This is the initial entrypoint for logging in. Take the response from this endpoint and give it to your user's wallet for singing. POST the response to `/user/login`.
///
/// Rate limited by IP address.
#[debug_handler]
pub async fn user_login_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
    // TODO: what does axum's error handling look like if the path fails to parse?
    // TODO: allow ENS names here?
    Path(mut params): Path<HashMap<String, String>>,
) -> FrontendResult {
    // give these named variables so that we drop them at the very end of this function
    let (_, _semaphore) = login_is_authorized(&app, ip).await?;

    // at first i thought about checking that user_address is in our db
    // but theres no need to separate the registration and login flows
    // its a better UX to just click "login with ethereum" and have the account created if it doesn't exist
    // we can prompt for an email and and payment after they log in

    // create a message and save it in redis

    // TODO: how many seconds? get from config?
    let expire_seconds: usize = 20 * 60;

    let nonce = Ulid::new();

    let issued_at = OffsetDateTime::now_utc();

    let expiration_time = issued_at.add(Duration::new(expire_seconds as i64, 0));

    let user_address: Address = params
        .remove("user_address")
        // TODO: map_err so this becomes a 500. routing must be bad
        .context("impossible")?
        .parse()
        // TODO: map_err so this becomes a 401
        .context("bad input")?;

    // TODO: get most of these from the app config
    let message = Message {
        // TODO: should domain be llamanodes, or llamarpc, or the subdomain of llamarpc?
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

    // TODO: if no redis server, store in local cache? at least give a better error. right now this seems to be a 502
    // the address isn't enough. we need to save the actual message so we can read the nonce
    // TODO: what message format is the most efficient to store in redis? probably eip191_bytes
    // we add 1 to expire_seconds just to be sure redis has the key for the full expiration_time
    // TODO: store a maximum number of attempted logins? anyone can request so we don't want to allow DOS attacks
    let session_key = format!("login_nonce:{}", nonce);
    app.redis_conn()
        .await?
        .set_ex(session_key, message.to_string(), expire_seconds + 1)
        .await?;

    // there are multiple ways to sign messages and not all wallets support them
    // TODO: default message eip from config?
    let message_eip = params
        .remove("message_eip")
        .unwrap_or_else(|| "eip4361".to_string());

    let message: String = match message_eip.as_str() {
        "eip191_bytes" => Bytes::from(message.eip191_bytes().unwrap()).to_string(),
        "eip191_hash" => Bytes::from(&message.eip191_hash().unwrap()).to_string(),
        "eip4361" => message.to_string(),
        _ => {
            // TODO: custom error that is handled a 401
            return Err(anyhow::anyhow!("invalid message eip given").into());
        }
    };

    Ok(message.into_response())
}

/// Query params for our `post_login` handler.
#[derive(Debug, Deserialize)]
pub struct PostLoginQuery {
    /// While we are in alpha/beta, we require users to supply an invite code.
    /// The invite code (if any) is set in the application's config.
    /// This may eventually provide some sort of referral bonus.
    pub invite_code: Option<String>,
}

/// JSON body to our `post_login` handler.
/// Currently only siwe logins that send an address, msg, and sig are allowed.
/// Email/password and other login methods are planned.
#[derive(Deserialize)]
pub struct PostLogin {
    sig: String,
    msg: String,
}

/// Successful logins receive a bearer_token and all of the user's api keys.
#[derive(Serialize)]
pub struct PostLoginResponse {
    /// Used for authenticating additonal requests.
    bearer_token: Ulid,
    /// Used for authenticating with the RPC endpoints.
    api_keys: Vec<UserKey>,
    user_id: u64,
    // TODO: what else?
}

/// `POST /user/login` - Register or login by posting a signed "siwe" message.
/// It is recommended to save the returned bearer token in a cookie.
/// The bearer token can be used to authenticate other requests, such as getting the user's stats or modifying the user's profile.
#[debug_handler]
pub async fn user_login_post(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
    Json(payload): Json<PostLogin>,
    Query(query): Query<PostLoginQuery>,
) -> FrontendResult {
    // give these named variables so that we drop them at the very end of this function
    let (_, _semaphore) = login_is_authorized(&app, ip).await?;

    if let Some(invite_code) = &app.config.invite_code {
        // we don't do per-user referral codes because we shouldn't collect what we don't need.
        // we don't need to build a social graph between addresses like that.
        if query.invite_code.as_ref() != Some(invite_code) {
            warn!("if address is already registered, allow login! else, error");

            // TODO: this return doesn't seem right
            return Err(anyhow::anyhow!("checking invite_code"))?;
        }
    }

    // we can't trust that they didn't tamper with the message in some way
    // TODO: it seems like some clients do things unexpectedly. these don't always parse
    // let their_msg: siwe::Message = payload.msg.parse().context("parsing user's message")?;

    // TODO: do this safely
    let their_sig_bytes = Bytes::from_str(&payload.sig).context("parsing sig")?;
    if their_sig_bytes.len() != 65 {
        return Err(anyhow::anyhow!("checking signature length"))?;
    }
    let mut their_sig: [u8; 65] = [0; 65];
    for x in 0..65 {
        their_sig[x] = their_sig_bytes[x]
    }

    let their_msg: String = if payload.msg.starts_with("0x") {
        let their_msg_bytes = Bytes::from_str(&payload.msg).context("parsing payload message")?;

        // TODO: lossy or no?
        String::from_utf8_lossy(their_msg_bytes.as_ref()).to_string()
    } else {
        payload.msg
    };

    let their_msg = their_msg
        .parse::<siwe::Message>()
        .context("parsing string message")?;

    // TODO: this is fragile
    let login_nonce_key = format!("login_nonce:{}", &their_msg.nonce);

    // fetch the message we gave them from our redis
    let mut redis_conn = app.redis_conn().await?;

    let our_msg: Option<String> = redis_conn.get(&login_nonce_key).await?;

    let our_msg: String = our_msg.context("login nonce not found")?;

    let our_msg: siwe::Message = our_msg.parse().context("parsing siwe message")?;

    // TODO: info for now
    info!(?our_msg, ?their_msg);

    // let timestamp be automatic
    // we don't need to check domain or nonce because we store the message safely locally
    let verify_config = VerificationOpts::default();

    // TODO: verify or verify_eip191?
    // TODO: save this when we save the message type to redis? we still need to check both
    if let Err(err_1) = our_msg
        .verify(&their_sig, &verify_config)
        .await
        .context("verifying signature against our local message")
    {
        // verification method 1 failed. try eip191
        if let Err(err_191) = our_msg
            .verify_eip191(&their_sig)
            .context("verifying eip191 signature against our local message")
        {
            return Err(anyhow::anyhow!(
                "both the primary and eip191 verification failed: {:#?}; {:#?}",
                err_1,
                err_191
            ))?;
        }
    }

    let bearer_token = Ulid::new();

    let db_conn = app.db_conn().context("Getting database connection")?;

    // TODO: limit columns or load whole user?
    let u = user::Entity::find()
        .filter(user::Column::Address.eq(our_msg.address.as_ref()))
        .one(&db_conn)
        .await
        .unwrap();

    let (u, _uks, response) = match u {
        None => {
            let txn = db_conn.begin().await?;

            // the only thing we need from them is an address
            // everything else is optional
            let u = user::ActiveModel {
                address: sea_orm::Set(our_msg.address.into()),
                ..Default::default()
            };

            let u = u.insert(&txn).await?;

            let user_key = UserKey::new();

            let uk = user_keys::ActiveModel {
                user_id: sea_orm::Set(u.id),
                api_key: sea_orm::Set(user_key.into()),
                requests_per_minute: sea_orm::Set(app.config.default_user_requests_per_minute),
                ..Default::default()
            };

            // TODO: if this fails, revert adding the user, too
            let uk = uk
                .insert(&txn)
                .await
                .context("Failed saving new user key")?;

            let uks = vec![uk];

            txn.commit().await?;

            let response_json = PostLoginResponse {
                bearer_token,
                api_keys: uks.iter().map(|uk| uk.api_key.into()).collect(),
                user_id: u.id,
            };

            let response = (StatusCode::CREATED, Json(response_json)).into_response();

            (u, uks, response)
        }
        Some(u) => {
            // the user is already registered
            let uks = user_keys::Entity::find()
                .filter(user_keys::Column::UserId.eq(u.id))
                .all(&db_conn)
                .await
                .context("failed loading user's key")?;

            let response_json = PostLoginResponse {
                bearer_token,
                api_keys: uks.iter().map(|uk| uk.api_key.into()).collect(),
                user_id: u.id,
            };

            let response = (StatusCode::OK, Json(response_json)).into_response();

            (u, uks, response)
        }
    };

    // add bearer to redis
    let bearer_redis_key = format!("bearer:{}", bearer_token);

    // expire in 4 weeks
    // TODO: do this with a pipe
    // TODO: get expiration time from app config
    // TODO: do we use this?
    redis_conn
        .set_ex(bearer_redis_key, u.id.to_string(), 2_419_200)
        .await?;

    if let Err(err) = redis_conn.del::<_, u64>(&login_nonce_key).await {
        warn!(
            "Failed to delete login_nonce_key {}: {}",
            login_nonce_key, err
        );
    }

    Ok(response)
}

/// `POST /user/logout` - Forget the bearer token in the `Authentication` header.
#[debug_handler]
pub async fn user_logout_post(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
) -> FrontendResult {
    let mut redis_conn = app.redis_conn().await?;

    // TODO: i don't like this. move this to a helper function so it is less fragile
    let bearer_cache_key = format!("bearer:{}", bearer.token());

    redis_conn.del(bearer_cache_key).await?;

    // TODO: what should the response be? probably json something
    Ok("goodbye".into_response())
}

/// the JSON input to the `post_user` handler
/// This handles updating
#[derive(Deserialize)]
pub struct PostUser {
    primary_address: Address,
    // TODO: make sure the email address is valid. probably have a "verified" column in the database
    email: Option<String>,
    // TODO: make them sign this JSON? cookie in session id is hard because its on a different domain
}

/// `POST /user/profile` -- modify the account connected to the bearer token in the `Authentication` header.
#[debug_handler]
pub async fn user_profile_post(
    TypedHeader(Authorization(bearer_token)): TypedHeader<Authorization<Bearer>>,
    ClientIp(ip): ClientIp,
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    Json(payload): Json<PostUser>,
) -> FrontendResult {
    // give these named variables so that we drop them at the very end of this function
    let (_, _semaphore) = login_is_authorized(&app, ip).await?;

    let user = ProtectedAction::PostUser(payload.primary_address)
        .verify(app.as_ref(), bearer_token)
        .await?;

    let mut user: user::ActiveModel = user.into();

    // TODO: rate limit by user, too?

    // TODO: allow changing the primary address, too. require a message from the new address to finish the change

    if let Some(x) = payload.email {
        // TODO: only Set if no change
        if x.is_empty() {
            user.email = sea_orm::Set(None);
        } else {
            user.email = sea_orm::Set(Some(x));
        }
    }

    let db_conn = app.db_conn().context("Getting database connection")?;

    user.save(&db_conn).await?;

    todo!("finish post_user");
}

/// `GET /user/balance` -- Use a bearer token to get the user's balance and spend.
///
/// - show balance in USD
/// - show deposits history (currency, amounts, transaction id)
///
/// TODO: one key per request? maybe /user/balance/:api_key?
/// TODO: this will change as we add better support for secondary users.
#[debug_handler]
pub async fn user_balance_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
) -> FrontendResult {
    let (authorized_request, _semaphore) = bearer_is_authorized(&app, bearer).await?;

    todo!("user_balance_get");
}

/// `POST /user/balance` -- Manually process a confirmed txid to update a user's balance.
///
/// We will subscribe to events to watch for any user deposits, but sometimes events can be missed.
///
/// TODO: rate limit by user
/// TODO: one key per request? maybe /user/balance/:api_key?
/// TODO: this will change as we add better support for secondary users.
#[debug_handler]
pub async fn user_balance_post(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer_token)): TypedHeader<Authorization<Bearer>>,
) -> FrontendResult {
    todo!("user_balance_post");
}

/// `GET /user/keys` -- Use a bearer token to get the user's api keys and their settings.
///
/// TODO: one key per request? maybe /user/keys/:api_key?
#[debug_handler]
pub async fn user_keys_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer_token)): TypedHeader<Authorization<Bearer>>,
) -> FrontendResult {
    todo!("user_keys_get");
}

/// `POST /user/keys` -- Use a bearer token to create a new key or modify an existing key.
///
/// TODO: read json from the request body
/// TODO: one key per request? maybe /user/keys/:api_key?
#[debug_handler]
pub async fn user_keys_post(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer_token)): TypedHeader<Authorization<Bearer>>,
) -> FrontendResult {
    todo!("user_keys_post");
}

/// `GET /user/profile` -- Use a bearer token to get the user's profile.
///
/// - the email address of a user if they opted in to get contacted via email
///
/// TODO: this will change as we add better support for secondary users.
#[debug_handler]
pub async fn user_profile_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer_token)): TypedHeader<Authorization<Bearer>>,
) -> FrontendResult {
    todo!("user_profile_get");
}

/// `GET /user/revert_logs` -- Use a bearer token to get the user's revert logs.
#[debug_handler]
pub async fn user_revert_logs_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer_token)): TypedHeader<Authorization<Bearer>>,
) -> FrontendResult {
    todo!("user_revert_logs_get");
}

/// `GET /user/stats/detailed` -- Use a bearer token to get the user's key stats such as bandwidth used and methods requested.
///
/// If no bearer is provided, detailed stats for all users will be shown.
/// View a single user with `?user_id=$x`.
/// View a single chain with `?chain_id=$x`.
///
/// Set `$x` to zero to see all.
///
/// TODO: this will change as we add better support for secondary users.
#[debug_handler]
pub async fn user_stats_detailed_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    Query(params): Query<HashMap<String, String>>,
) -> FrontendResult {
    let x = get_detailed_stats(&app, bearer, params).await?;

    Ok(Json(x).into_response())
}

/// `GET /user/stats/aggregate` -- Public endpoint for aggregate stats such as bandwidth used and methods requested.
#[debug_handler]
pub async fn user_stats_aggregate_get(
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    Query(params): Query<HashMap<String, String>>,
) -> FrontendResult {
    let x = get_aggregate_rpc_stats_from_params(&app, bearer, params).await?;

    Ok(Json(x).into_response())
}

/// `GET /user/profile` -- Use a bearer token to get the user's profile such as their optional email address.
/// Handle authorization for a given address and bearer token.
// TODO: what roles should exist?
enum ProtectedAction {
    PostUser(Address),
}

impl ProtectedAction {
    /// Verify that the given bearer token and address are allowed to take the specified action.
    async fn verify(
        self,
        app: &Web3ProxyApp,
        // TODO: i don't think we want Bearer here. we want user_key and a helper for bearer -> user_key
        bearer: Bearer,
    ) -> anyhow::Result<user::Model> {
        // get the attached address from redis for the given auth_token.
        let mut redis_conn = app.redis_conn().await?;

        // TODO: move this to a helper function
        let bearer_cache_key = format!("bearer:{}", bearer.token());

        // TODO: move this to a helper function
        let user_id: Option<u64> = redis_conn
            .get(bearer_cache_key)
            .await
            .context("fetching bearer cache key from redis")?;

        // TODO: if auth_address == primary_address, allow
        // TODO: if auth_address != primary_address, only allow if they are a secondary user with the correct role
        todo!("verify token for the given user");
    }
}
