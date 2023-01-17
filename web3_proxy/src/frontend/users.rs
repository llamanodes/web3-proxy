//! Handle registration, logins, and managing account data.

use super::authorization::{login_is_authorized, RpcSecretKey};
use super::errors::FrontendResult;
use crate::app::Web3ProxyApp;
use crate::user_queries::{get_page_from_params, get_user_id_from_params};
use crate::user_queries::{
    get_chain_id_from_params, get_query_start_from_params, query_user_stats, StatResponse,
};
use entities::prelude::{User, SecondaryUser};
use crate::user_token::UserBearerToken;
use anyhow::Context;
use axum::headers::{Header, Origin, Referer, UserAgent};
use axum::{
    extract::{Path, Query},
    headers::{authorization::Bearer, Authorization},
    response::IntoResponse,
    Extension, Json, TypedHeader,
};
use axum_client_ip::ClientIp;
use axum_macros::debug_handler;
use chrono::{TimeZone, Utc};
use entities::sea_orm_active_enums::{LogLevel, Role};
use entities::{login, pending_login, revert_log, rpc_key, secondary_user, user, user_tier};
use ethers::{prelude::Address, types::Bytes};
use hashbrown::HashMap;
use http::{HeaderValue, StatusCode};
use ipnet::IpNet;
use itertools::Itertools;
use log::{debug, info, warn};
use migration::sea_orm::prelude::Uuid;
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, PaginatorTrait, QueryFilter,
    QueryOrder, TransactionTrait, TryIntoModel,
};
use serde::Deserialize;
use serde_json::json;
use siwe::{Message, VerificationOpts};
use std::ops::Add;
use std::str::FromStr;
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
use ulid::Ulid;
use crate::admin_queries::query_admin_modify_usertier;
use crate::frontend::errors::FrontendErrorResponse;

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
///
/// At first i thought about checking that user_address is in our db,
/// But theres no need to separate the registration and login flows.
/// It is a better UX to just click "login with ethereum" and have the account created if it doesn't exist.
/// We can prompt for an email and and payment after they log in.
#[debug_handler]
pub async fn user_login_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
    // TODO: what does axum's error handling look like if the path fails to parse?
    Path(mut params): Path<HashMap<String, String>>,
) -> FrontendResult {
    login_is_authorized(&app, ip).await?;

    // create a message and save it in redis
    // TODO: how many seconds? get from config?
    let expire_seconds: usize = 20 * 60;

    let nonce = Ulid::new();

    let issued_at = OffsetDateTime::now_utc();

    let expiration_time = issued_at.add(Duration::new(expire_seconds as i64, 0));

    // TODO: allow ENS names here?
    let user_address: Address = params
        .remove("user_address")
        // TODO: map_err so this becomes a 500. routing must be bad
        .context("impossible")?
        .parse()
        // TODO: map_err so this becomes a 401
        .context("unable to parse address")?;

    let login_domain = app
        .config
        .login_domain
        .clone()
        .unwrap_or_else(|| "llamanodes.com".to_string());

    // TODO: get most of these from the app config
    let message = Message {
        // TODO: don't unwrap
        // TODO: accept a login_domain from the request?
        domain: login_domain.parse().unwrap(),
        address: user_address.to_fixed_bytes(),
        // TODO: config for statement
        statement: Some("🦙🦙🦙🦙🦙".to_string()),
        // TODO: don't unwrap
        uri: format!("https://{}/", login_domain).parse().unwrap(),
        version: siwe::Version::V1,
        chain_id: 1,
        expiration_time: Some(expiration_time.into()),
        issued_at: issued_at.into(),
        nonce: nonce.to_string(),
        not_before: None,
        request_id: None,
        resources: vec![],
    };

    let db_conn = app.db_conn().context("login requires a database")?;

    // massage types to fit in the database. sea-orm does not make this very elegant
    let uuid = Uuid::from_u128(nonce.into());
    // we add 1 to expire_seconds just to be sure the database has the key for the full expiration_time
    let expires_at = Utc
        .timestamp_opt(expiration_time.unix_timestamp() + 1, 0)
        .unwrap();

    // we do not store a maximum number of attempted logins. anyone can request so we don't want to allow DOS attacks
    // add a row to the database for this user
    let user_pending_login = pending_login::ActiveModel {
        id: sea_orm::NotSet,
        nonce: sea_orm::Set(uuid),
        message: sea_orm::Set(message.to_string()),
        expires_at: sea_orm::Set(expires_at),
    };

    user_pending_login
        .save(&db_conn)
        .await
        .context("saving user's pending_login")?;

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
#[derive(Debug, Deserialize)]
pub struct PostLogin {
    sig: String,
    msg: String,
}

/// `POST /user/login` - Register or login by posting a signed "siwe" message.
/// It is recommended to save the returned bearer token in a cookie.
/// The bearer token can be used to authenticate other requests, such as getting the user's stats or modifying the user's profile.
#[debug_handler]
pub async fn user_login_post(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ClientIp(ip): ClientIp,
    Query(query): Query<PostLoginQuery>,
    Json(payload): Json<PostLogin>,
) -> FrontendResult {
    login_is_authorized(&app, ip).await?;

    // TODO: this seems too verbose. how can we simply convert a String into a [u8; 65]
    let their_sig_bytes = Bytes::from_str(&payload.sig).context("parsing sig")?;
    if their_sig_bytes.len() != 65 {
        return Err(anyhow::anyhow!("checking signature length").into());
    }
    let mut their_sig: [u8; 65] = [0; 65];
    for x in 0..65 {
        their_sig[x] = their_sig_bytes[x]
    }

    // we can't trust that they didn't tamper with the message in some way. like some clients return it hex encoded
    // TODO: checking 0x seems fragile, but I think it will be fine. siwe message text shouldn't ever start with 0x
    let their_msg: Message = if payload.msg.starts_with("0x") {
        let their_msg_bytes = Bytes::from_str(&payload.msg).context("parsing payload message")?;

        // TODO: lossy or no?
        String::from_utf8_lossy(their_msg_bytes.as_ref())
            .parse::<siwe::Message>()
            .context("parsing hex string message")?
    } else {
        payload
            .msg
            .parse::<siwe::Message>()
            .context("parsing string message")?
    };

    // the only part of the message we will trust is their nonce
    // TODO: this is fragile. have a helper function/struct for redis keys
    let login_nonce = UserBearerToken::from_str(&their_msg.nonce)?;

    // fetch the message we gave them from our database
    let db_replica = app.db_replica().context("Getting database connection")?;

    // massage type for the db
    let login_nonce_uuid: Uuid = login_nonce.clone().into();

    let user_pending_login = pending_login::Entity::find()
        .filter(pending_login::Column::Nonce.eq(login_nonce_uuid))
        .one(db_replica.conn())
        .await
        .context("database error while finding pending_login")?
        .context("login nonce not found")?;

    let our_msg: siwe::Message = user_pending_login
        .message
        .parse()
        .context("parsing siwe message")?;

    // default options are fine. the message includes timestamp and domain and nonce
    let verify_config = VerificationOpts::default();

    // Check with both verify and verify_eip191
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
            let db_conn = app
                .db_conn()
                .context("deleting expired pending logins requires a db")?;

            // delete ALL expired rows.
            let now = Utc::now();
            let delete_result = pending_login::Entity::delete_many()
                .filter(pending_login::Column::ExpiresAt.lte(now))
                .exec(&db_conn)
                .await?;

            // TODO: emit a stat? if this is high something weird might be happening
            debug!("cleared expired pending_logins: {:?}", delete_result);

            return Err(anyhow::anyhow!(
                "both the primary and eip191 verification failed: {:#?}; {:#?}",
                err_1,
                err_191
            )
            .into());
        }
    }

    // TODO: limit columns or load whole user?
    let u = user::Entity::find()
        .filter(user::Column::Address.eq(our_msg.address.as_ref()))
        .one(db_replica.conn())
        .await
        .unwrap();

    let db_conn = app.db_conn().context("login requires a db")?;

    let (u, uks, status_code) = match u {
        None => {
            // user does not exist yet

            // check the invite code
            // TODO: more advanced invite codes that set different request/minute and concurrency limits
            if let Some(invite_code) = &app.config.invite_code {
                if query.invite_code.as_ref() != Some(invite_code) {
                    return Err(anyhow::anyhow!("checking invite_code").into());
                }
            }

            let txn = db_conn.begin().await?;

            // the only thing we need from them is an address
            // everything else is optional
            // TODO: different invite codes should allow different levels
            // TODO: maybe decrement a count on the invite code?
            let u = user::ActiveModel {
                address: sea_orm::Set(our_msg.address.into()),
                ..Default::default()
            };

            let u = u.insert(&txn).await?;

            // create the user's first api key
            let rpc_secret_key = RpcSecretKey::new();

            let uk = rpc_key::ActiveModel {
                user_id: sea_orm::Set(u.id),
                secret_key: sea_orm::Set(rpc_secret_key.into()),
                description: sea_orm::Set(None),
                ..Default::default()
            };

            let uk = uk
                .insert(&txn)
                .await
                .context("Failed saving new user key")?;

            let uks = vec![uk];

            // save the user and key to the database
            txn.commit().await?;

            (u, uks, StatusCode::CREATED)
        }
        Some(u) => {
            // the user is already registered
            let uks = rpc_key::Entity::find()
                .filter(rpc_key::Column::UserId.eq(u.id))
                .all(db_replica.conn())
                .await
                .context("failed loading user's key")?;

            (u, uks, StatusCode::OK)
        }
    };

    // create a bearer token for the user.
    let user_bearer_token = UserBearerToken::default();

    // json response with everything in it
    // we could return just the bearer token, but I think they will always request api keys and the user profile
    let response_json = json!({
        "rpc_keys": uks
            .into_iter()
            .map(|uk| (uk.id, uk))
            .collect::<HashMap<_, _>>(),
        "bearer_token": user_bearer_token,
        "user": u,
    });

    let response = (status_code, Json(response_json)).into_response();

    // add bearer to the database

    // expire in 4 weeks
    let expires_at = Utc::now()
        .checked_add_signed(chrono::Duration::weeks(4))
        .unwrap();

    let user_login = login::ActiveModel {
        id: sea_orm::NotSet,
        bearer_token: sea_orm::Set(user_bearer_token.uuid()),
        user_id: sea_orm::Set(u.id),
        expires_at: sea_orm::Set(expires_at),
    };

    user_login
        .save(&db_conn)
        .await
        .context("saving user login")?;

    if let Err(err) = user_pending_login
        .into_active_model()
        .delete(&db_conn)
        .await
    {
        warn!("Failed to delete nonce:{}: {}", login_nonce.0, err);
    }

    Ok(response)
}

/// `POST /user/logout` - Forget the bearer token in the `Authentication` header.
#[debug_handler]
pub async fn user_logout_post(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
) -> FrontendResult {
    let user_bearer = UserBearerToken::try_from(bearer)?;

    let db_conn = app.db_conn().context("database needed for user logout")?;

    if let Err(err) = login::Entity::delete_many()
        .filter(login::Column::BearerToken.eq(user_bearer.uuid()))
        .exec(&db_conn)
        .await
    {
        debug!("Failed to delete {}: {}", user_bearer.redis_key(), err);
    }

    let now = Utc::now();

    // also delete any expired logins
    let delete_result = login::Entity::delete_many()
        .filter(login::Column::ExpiresAt.lte(now))
        .exec(&db_conn)
        .await;

    debug!("Deleted expired logins: {:?}", delete_result);

    // also delete any expired pending logins
    let delete_result = login::Entity::delete_many()
        .filter(login::Column::ExpiresAt.lte(now))
        .exec(&db_conn)
        .await;

    debug!("Deleted expired pending logins: {:?}", delete_result);

    // TODO: what should the response be? probably json something
    Ok("goodbye".into_response())
}

/// `GET /user` -- Use a bearer token to get the user's profile.
///
/// - the email address of a user if they opted in to get contacted via email
///
/// TODO: this will change as we add better support for secondary users.
#[debug_handler]
pub async fn user_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer_token)): TypedHeader<Authorization<Bearer>>,
) -> FrontendResult {
    let (user, _semaphore) = app.bearer_is_authorized(bearer_token).await?;

    Ok(Json(user).into_response())
}

/// the JSON input to the `post_user` handler.
#[derive(Debug, Deserialize)]
pub struct UserPost {
    email: Option<String>,
}

/// `POST /user` -- modify the account connected to the bearer token in the `Authentication` header.
#[debug_handler]
pub async fn user_post(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer_token)): TypedHeader<Authorization<Bearer>>,
    Json(payload): Json<UserPost>,
) -> FrontendResult {
    let (user, _semaphore) = app.bearer_is_authorized(bearer_token).await?;

    let mut user: user::ActiveModel = user.into();

    // update the email address
    if let Some(x) = payload.email {
        // TODO: only Set if no change
        if x.is_empty() {
            user.email = sea_orm::Set(None);
        } else {
            // TODO: do some basic validation
            // TODO: don't set immediatly, send a confirmation email first
            // TODO: compare first? or is sea orm smart enough to do that for us?
            user.email = sea_orm::Set(Some(x));
        }
    }

    // TODO: what else can we update here? password hash? subscription to newsletter?

    let user = if user.is_changed() {
        let db_conn = app.db_conn().context("Getting database connection")?;

        user.save(&db_conn).await?
    } else {
        // no changes. no need to touch the database
        user
    };

    let user: user::Model = user.try_into().context("Returning updated user")?;

    Ok(Json(user).into_response())
}

/// `GET /user/balance` -- Use a bearer token to get the user's balance and spend.
///
/// - show balance in USD
/// - show deposits history (currency, amounts, transaction id)
///
/// TODO: one key per request? maybe /user/balance/:rpc_key?
/// TODO: this will change as we add better support for secondary users.
#[debug_handler]
pub async fn user_balance_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
) -> FrontendResult {
    let (user, _semaphore) = app.bearer_is_authorized(bearer).await?;

    todo!("user_balance_get");
}

/// `POST /user/balance/:txhash` -- Manually process a confirmed txid to update a user's balance.
///
/// We will subscribe to events to watch for any user deposits, but sometimes events can be missed.
///
/// TODO: rate limit by user
/// TODO: one key per request? maybe /user/balance/:rpc_key?
/// TODO: this will change as we add better support for secondary users.
#[debug_handler]
pub async fn user_balance_post(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
) -> FrontendResult {
    let (user, _semaphore) = app.bearer_is_authorized(bearer).await?;

    todo!("user_balance_post");
}

/// `GET /user/keys` -- Use a bearer token to get the user's api keys and their settings.
///
/// TODO: one key per request? maybe /user/keys/:rpc_key?
#[debug_handler]
pub async fn rpc_keys_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
) -> FrontendResult {
    let (user, _semaphore) = app.bearer_is_authorized(bearer).await?;

    let db_replica = app
        .db_replica()
        .context("getting db to fetch user's keys")?;

    let uks = rpc_key::Entity::find()
        .filter(rpc_key::Column::UserId.eq(user.id))
        .all(db_replica.conn())
        .await
        .context("failed loading user's key")?;

    // TODO: stricter type on this?
    let response_json = json!({
        "user_id": user.id,
        "user_rpc_keys": uks
            .into_iter()
            .map(|uk| (uk.id, uk))
            .collect::<HashMap::<_, _>>(),
    });

    Ok(Json(response_json).into_response())
}

/// `DELETE /user/keys` -- Use a bearer token to delete an existing key.
#[debug_handler]
pub async fn rpc_keys_delete(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
) -> FrontendResult {
    let (user, _semaphore) = app.bearer_is_authorized(bearer).await?;

    // TODO: think about how cascading deletes and billing should work
    Err(anyhow::anyhow!("work in progress").into())
}

/// the JSON input to the `rpc_keys_management` handler.
/// If `key_id` is set, it updates an existing key.
/// If `key_id` is not set, it creates a new key.
/// `log_request_method` cannot be change once the key is created
/// `user_tier` cannot be changed here
#[derive(Debug, Deserialize)]
pub struct UserKeyManagement {
    key_id: Option<u64>,
    active: Option<bool>,
    allowed_ips: Option<String>,
    allowed_origins: Option<String>,
    allowed_referers: Option<String>,
    allowed_user_agents: Option<String>,
    description: Option<String>,
    log_level: Option<LogLevel>,
    // TODO: enable log_revert_trace: Option<f64>,
    private_txs: Option<bool>,
}

/// `POST /user/keys` or `PUT /user/keys` -- Use a bearer token to create or update an existing key.
#[debug_handler]
pub async fn rpc_keys_management(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
    Json(payload): Json<UserKeyManagement>,
) -> FrontendResult {
    // TODO: is there a way we can know if this is a PUT or POST? right now we can modify or create keys with either. though that probably doesn't matter

    let (user, _semaphore) = app.bearer_is_authorized(bearer).await?;

    let db_replica = app.db_replica().context("getting db for user's keys")?;

    let mut uk = if let Some(existing_key_id) = payload.key_id {
        // get the key and make sure it belongs to the user
        rpc_key::Entity::find()
            .filter(rpc_key::Column::UserId.eq(user.id))
            .filter(rpc_key::Column::Id.eq(existing_key_id))
            .one(db_replica.conn())
            .await
            .context("failed loading user's key")?
            .context("key does not exist or is not controlled by this bearer token")?
            .into_active_model()
    } else {
        // make a new key
        // TODO: limit to 10 keys?
        let secret_key = RpcSecretKey::new();

        let log_level = payload
            .log_level
            .context("log level must be 'none', 'detailed', or 'aggregated'")?;

        rpc_key::ActiveModel {
            user_id: sea_orm::Set(user.id),
            secret_key: sea_orm::Set(secret_key.into()),
            log_level: sea_orm::Set(log_level),
            ..Default::default()
        }
    };

    // TODO: do we need null descriptions? default to empty string should be fine, right?
    if let Some(description) = payload.description {
        if description.is_empty() {
            uk.description = sea_orm::Set(None);
        } else {
            uk.description = sea_orm::Set(Some(description));
        }
    }

    if let Some(private_txs) = payload.private_txs {
        uk.private_txs = sea_orm::Set(private_txs);
    }

    if let Some(active) = payload.active {
        uk.active = sea_orm::Set(active);
    }

    if let Some(allowed_ips) = payload.allowed_ips {
        if allowed_ips.is_empty() {
            uk.allowed_ips = sea_orm::Set(None);
        } else {
            // split allowed ips on ',' and try to parse them all. error on invalid input
            let allowed_ips = allowed_ips
                .split(',')
                .map(|x| x.trim().parse::<IpNet>())
                .collect::<Result<Vec<_>, _>>()?
                // parse worked. convert back to Strings
                .into_iter()
                .map(|x| x.to_string());

            // and join them back together
            let allowed_ips: String =
                Itertools::intersperse(allowed_ips, ", ".to_string()).collect();

            uk.allowed_ips = sea_orm::Set(Some(allowed_ips));
        }
    }

    // TODO: this should actually be bytes
    if let Some(allowed_origins) = payload.allowed_origins {
        if allowed_origins.is_empty() {
            uk.allowed_origins = sea_orm::Set(None);
        } else {
            // split allowed_origins on ',' and try to parse them all. error on invalid input
            let allowed_origins = allowed_origins
                .split(',')
                .map(|x| HeaderValue::from_str(x.trim()))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .map(|x| Origin::decode(&mut [x].iter()))
                .collect::<Result<Vec<_>, _>>()?
                // parse worked. convert back to String and join them back together
                .into_iter()
                .map(|x| x.to_string());

            let allowed_origins: String =
                Itertools::intersperse(allowed_origins, ", ".to_string()).collect();

            uk.allowed_origins = sea_orm::Set(Some(allowed_origins));
        }
    }

    // TODO: this should actually be bytes
    if let Some(allowed_referers) = payload.allowed_referers {
        if allowed_referers.is_empty() {
            uk.allowed_referers = sea_orm::Set(None);
        } else {
            // split allowed ips on ',' and try to parse them all. error on invalid input
            let allowed_referers = allowed_referers
                .split(',')
                .map(|x| HeaderValue::from_str(x.trim()))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .map(|x| Referer::decode(&mut [x].iter()))
                .collect::<Result<Vec<_>, _>>()?;

            // parse worked. now we can put it back together.
            // but we can't go directly to String.
            // so we convert to HeaderValues first
            let mut header_map = vec![];
            for x in allowed_referers {
                x.encode(&mut header_map);
            }

            // convert HeaderValues to Strings
            // since we got these from strings, this should always work (unless we figure out using bytes)
            let allowed_referers = header_map
                .into_iter()
                .map(|x| x.to_str().map(|x| x.to_string()))
                .collect::<Result<Vec<_>, _>>()?;

            // join strings together with commas
            let allowed_referers: String =
                Itertools::intersperse(allowed_referers.into_iter(), ", ".to_string()).collect();

            uk.allowed_referers = sea_orm::Set(Some(allowed_referers));
        }
    }

    if let Some(allowed_user_agents) = payload.allowed_user_agents {
        if allowed_user_agents.is_empty() {
            uk.allowed_user_agents = sea_orm::Set(None);
        } else {
            // split allowed_user_agents on ',' and try to parse them all. error on invalid input
            let allowed_user_agents = allowed_user_agents
                .split(',')
                .filter_map(|x| x.trim().parse::<UserAgent>().ok())
                // parse worked. convert back to String
                .map(|x| x.to_string());

            // join the strings together
            let allowed_user_agents: String =
                Itertools::intersperse(allowed_user_agents, ", ".to_string()).collect();

            uk.allowed_user_agents = sea_orm::Set(Some(allowed_user_agents));
        }
    }

    let uk = if uk.is_changed() {
        let db_conn = app.db_conn().context("login requires a db")?;

        uk.save(&db_conn).await.context("Failed saving user key")?
    } else {
        uk
    };

    let uk = uk.try_into_model()?;

    Ok(Json(uk).into_response())
}

/// `GET /user/revert_logs` -- Use a bearer token to get the user's revert logs.
#[debug_handler]
pub async fn user_revert_logs_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
    Query(params): Query<HashMap<String, String>>,
) -> FrontendResult {
    let (user, _semaphore) = app.bearer_is_authorized(bearer).await?;

    let chain_id = get_chain_id_from_params(app.as_ref(), &params)?;
    let query_start = get_query_start_from_params(&params)?;
    let page = get_page_from_params(&params)?;

    // TODO: page size from config
    let page_size = 1_000;

    let mut response = HashMap::new();

    response.insert("page", json!(page));
    response.insert("page_size", json!(page_size));
    response.insert("chain_id", json!(chain_id));
    response.insert("query_start", json!(query_start.timestamp() as u64));

    let db_replica = app
        .db_replica()
        .context("getting replica db for user's revert logs")?;

    let uks = rpc_key::Entity::find()
        .filter(rpc_key::Column::UserId.eq(user.id))
        .all(db_replica.conn())
        .await
        .context("failed loading user's key")?;

    // TODO: only select the ids
    let uks: Vec<_> = uks.into_iter().map(|x| x.id).collect();

    // get revert logs
    let mut q = revert_log::Entity::find()
        .filter(revert_log::Column::Timestamp.gte(query_start))
        .filter(revert_log::Column::RpcKeyId.is_in(uks))
        .order_by_asc(revert_log::Column::Timestamp);

    if chain_id == 0 {
        // don't do anything
    } else {
        // filter on chain id
        q = q.filter(revert_log::Column::ChainId.eq(chain_id))
    }

    // query the database for number of items and pages
    let pages_result = q
        .clone()
        .paginate(db_replica.conn(), page_size)
        .num_items_and_pages()
        .await?;

    response.insert("num_items", pages_result.number_of_items.into());
    response.insert("num_pages", pages_result.number_of_pages.into());

    // query the database for the revert logs
    let revert_logs = q
        .paginate(db_replica.conn(), page_size)
        .fetch_page(page)
        .await?;

    response.insert("revert_logs", json!(revert_logs));

    Ok(Json(response).into_response())
}

/// `GET /user/stats/aggregate` -- Public endpoint for aggregate stats such as bandwidth used and methods requested.
#[debug_handler]
pub async fn user_stats_aggregated_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    Query(params): Query<HashMap<String, String>>,
) -> FrontendResult {
    let response = query_user_stats(&app, bearer, &params, StatResponse::Aggregated).await?;

    Ok(response)
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
    let response = query_user_stats(&app, bearer, &params, StatResponse::Detailed).await?;

    Ok(response)
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
pub async fn admin_change_user_roles(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    Query(params): Query<HashMap<String, String>>,
) -> FrontendResult {
    let response = query_admin_modify_usertier(&app, bearer, &params).await?;

    response
}
