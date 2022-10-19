//! Handle registration, logins, and managing account data.

use super::authorization::{login_is_authorized, UserKey};
use super::errors::FrontendResult;
use crate::app::Web3ProxyApp;
use anyhow::Context;
use axum::{
    extract::{Path, Query},
    headers::{authorization::Bearer, Authorization},
    response::IntoResponse,
    Extension, Json, TypedHeader,
};
use axum_client_ip::ClientIp;
use axum_macros::debug_handler;
use entities::{rpc_accounting, user, user_keys};
use ethers::{prelude::Address, types::Bytes};
use hashbrown::HashMap;
use http::StatusCode;
use redis_rate_limiter::redis::AsyncCommands;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, EntityTrait, QueryFilter, QuerySelect,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};
use siwe::{Message, VerificationOpts};
use std::ops::Add;
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
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
    // TODO: while developing, we put a giant number here
    let expire_seconds: usize = 28800;

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
        // TODO: should domain be llamanodes, or llamarpc?
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

    // TODO: if no redis server, store in local cache? at least give a better error. right now this seems to be a 502
    // the address isn't enough. we need to save the actual message so we can read the nonce
    // TODO: what message format is the most efficient to store in redis? probably eip191_bytes
    // we add 1 to expire_seconds just to be sure redis has the key for the full expiration_time
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
    pub address: Address,
    pub msg: String,
    pub sig: Bytes,
    // TODO: do we care about these? we should probably check the version is something we expect
    // version: String,
    // signer: String,
}

/// Successful logins receive a bearer_token and all of the user's api keys.
#[derive(Serialize)]
pub struct PostLoginResponse {
    /// Used for authenticating additonal requests.
    bearer_token: Ulid,
    /// Used for authenticating with the RPC endpoints.
    api_keys: Vec<UserKey>,
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
            todo!("if address is already registered, allow login! else, error")
        }
    }

    // we can't trust that they didn't tamper with the message in some way
    let their_msg: siwe::Message = payload.msg.parse().unwrap();

    let their_sig: [u8; 65] = payload.sig.as_ref().try_into().unwrap();

    // fetch the message we gave them from our redis
    // TODO: use getdel
    let our_msg: String = app.redis_conn().await?.get(&their_msg.nonce).await?;

    let our_msg: siwe::Message = our_msg.parse().unwrap();

    let verify_config = VerificationOpts {
        domain: Some(our_msg.domain),
        nonce: Some(our_msg.nonce),
        ..Default::default()
    };

    // check the domain and a nonce. let timestamp be automatic
    if let Err(e) = their_msg.verify(&their_sig, &verify_config).await {
        // message cannot be correctly authenticated
        todo!("proper error message: {}", e)
    }

    let bearer_token = Ulid::new();

    let db = app.db_conn().context("Getting database connection")?;

    // TODO: limit columns or load whole user?
    let u = user::Entity::find()
        .filter(user::Column::Address.eq(our_msg.address.as_ref()))
        .one(&db)
        .await
        .unwrap();

    let (u, _uks, response) = match u {
        None => {
            let txn = db.begin().await?;

            // the only thing we need from them is an address
            // everything else is optional
            let u = user::ActiveModel {
                address: sea_orm::Set(payload.address.to_fixed_bytes().into()),
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
            };

            let response = (StatusCode::CREATED, Json(response_json)).into_response();

            (u, uks, response)
        }
        Some(u) => {
            // the user is already registered
            let uks = user_keys::Entity::find()
                .filter(user_keys::Column::UserId.eq(u.id))
                .all(&db)
                .await
                .context("failed loading user's key")?;

            let response_json = PostLoginResponse {
                bearer_token,
                api_keys: uks.iter().map(|uk| uk.api_key.into()).collect(),
            };

            let response = (StatusCode::OK, Json(response_json)).into_response();

            (u, uks, response)
        }
    };

    // add bearer to redis
    let mut redis_conn = app.redis_conn().await?;

    let bearer_redis_key = format!("bearer:{}", bearer_token);

    // expire in 4 weeks
    // TODO: get expiration time from app config
    // TODO: do we use this?
    redis_conn
        .set_ex(bearer_redis_key, u.id.to_string(), 2_419_200)
        .await?;

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

    let user = ProtectedAction::PostUser
        .verify(app.as_ref(), bearer_token, &payload.primary_address)
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

    let db = app.db_conn().context("Getting database connection")?;

    user.save(&db).await?;

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
    TypedHeader(Authorization(bearer_token)): TypedHeader<Authorization<Bearer>>,
    Extension(app): Extension<Arc<Web3ProxyApp>>,
) -> FrontendResult {
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
    TypedHeader(Authorization(bearer_token)): TypedHeader<Authorization<Bearer>>,
    Extension(app): Extension<Arc<Web3ProxyApp>>,
) -> FrontendResult {
    todo!("user_balance_post");
}

/// `GET /user/keys` -- Use a bearer token to get the user's api keys and their settings.
///
/// TODO: one key per request? maybe /user/keys/:api_key?
#[debug_handler]
pub async fn user_keys_get(
    TypedHeader(Authorization(bearer_token)): TypedHeader<Authorization<Bearer>>,
    Extension(app): Extension<Arc<Web3ProxyApp>>,
) -> FrontendResult {
    todo!("user_keys_get");
}

/// `POST /user/keys` -- Use a bearer token to create a new key or modify an existing key.
///
/// TODO: read json from the request body
/// TODO: one key per request? maybe /user/keys/:api_key?
#[debug_handler]
pub async fn user_keys_post(
    TypedHeader(Authorization(bearer_token)): TypedHeader<Authorization<Bearer>>,
    Extension(app): Extension<Arc<Web3ProxyApp>>,
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
    TypedHeader(Authorization(bearer_token)): TypedHeader<Authorization<Bearer>>,
    Extension(app): Extension<Arc<Web3ProxyApp>>,
) -> FrontendResult {
    todo!("user_profile_get");
}

/// `GET /user/stats` -- Use a bearer token to get the user's key stats such as bandwidth used and methods requested.
///
/// - show number of requests used (so we can calculate average spending over a month, burn rate for a user etc, something like "Your balance will be depleted in xx days)
///
/// TODO: one key per request? maybe /user/stats/:api_key?
/// TODO: this will change as we add better support for secondary users.
#[debug_handler]
pub async fn user_stats_get(
    TypedHeader(Authorization(bearer_token)): TypedHeader<Authorization<Bearer>>,
    Extension(app): Extension<Arc<Web3ProxyApp>>,
) -> FrontendResult {
    todo!("user_stats_get");
}

/// `GET /user/stats/aggregate` -- Public endpoint for aggregate stats such as bandwidth used and methods requested.
#[debug_handler]
pub async fn user_stats_aggregate_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    Path(mut params): Path<HashMap<String, String>>,
) -> FrontendResult {
    // TODO: how is this supposed to be used?
    let db = app.db_conn.clone().context("no db")?;

    // TODO: read this from params
    let timeframe = chrono::Utc::now() - chrono::Duration::days(30);

    // TODO: how do we get count reverts compared to other errors? does it matter? what about http errors to our users?
    // TODO: how do we count uptime?
    let x = rpc_accounting::Entity::find()
        .select_only()
        .column_as(
            rpc_accounting::Column::FrontendRequests.sum(),
            "total_requests",
        )
        .column_as(
            rpc_accounting::Column::CacheMisses.sum(),
            "total_cache_misses",
        )
        .column_as(rpc_accounting::Column::CacheHits.sum(), "total_cache_hits")
        .column_as(
            rpc_accounting::Column::BackendRetries.sum(),
            "total_backend_retries",
        )
        .column_as(
            rpc_accounting::Column::SumResponseBytes.sum(),
            "total_response_bytes",
        )
        .column_as(
            // TODO: can we sum bools like this?
            rpc_accounting::Column::ErrorResponse.sum(),
            "total_error_responses",
        )
        .column_as(
            rpc_accounting::Column::SumResponseMillis.sum(),
            "total_response_millis",
        )
        .filter(
            Condition::all()
                .add(rpc_accounting::Column::ChainId.eq(app.config.chain_id))
                .add(rpc_accounting::Column::PeriodDatetime.gte(timeframe)),
        )
        .into_json()
        .one(&db)
        .await?
        .unwrap();

    Ok(Json(x).into_response())
}

/// `GET /user/profile` -- Use a bearer token to get the user's profile such as their optional email address.
/// Handle authorization for a given address and bearer token.
// TODO: what roles should exist?
enum ProtectedAction {
    PostUser,
}

impl ProtectedAction {
    /// Verify that the given bearer token and address are allowed to take the specified action.
    async fn verify(
        self,
        app: &Web3ProxyApp,
        // TODO: i don't think we want Bearer here. we want user_key and a helper for bearer -> user_key
        bearer: Bearer,
        // TODO: what about secondary addresses? maybe an enum for primary or secondary?
        primary_address: &Address,
    ) -> anyhow::Result<user::Model> {
        // get the attached address from redis for the given auth_token.
        let mut redis_conn = app.redis_conn().await?;

        let bearer_cache_key = format!("bearer:{}", bearer.token());

        let user_key_id: Option<u64> = redis_conn
            .get(bearer_cache_key)
            .await
            .context("fetching bearer cache key from redis")?;

        // TODO: if auth_address == primary_address, allow
        // TODO: if auth_address != primary_address, only allow if they are a secondary user with the correct role
        todo!("verify token for the given user");
    }
}
