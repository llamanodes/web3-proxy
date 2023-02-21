//! Handle admin helper logic

use super::authorization::{login_is_authorized, RpcSecretKey};
use super::errors::FrontendResult;
use crate::app::Web3ProxyApp;
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
use axum_client_ip::InsecureClientIp;
use axum_macros::debug_handler;
use chrono::{TimeZone, Utc};
use entities::sea_orm_active_enums::{Role};
use entities::{admin, admin_trail, login, pending_login, revert_log, rpc_key, secondary_user, user, user_tier};
use ethers::{abi::AbiEncode, prelude::Address, types::Bytes};
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
use crate::{PostLogin, PostLoginQuery};

/// `GET /admin/modify_role` -- As an admin, modify a user's user-tier
///
/// - user_address that is to be modified
/// - user_role_tier that is supposed to be adapted
#[debug_handler]
pub async fn admin_change_user_roles(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    Query(params): Query<HashMap<String, String>>,
) -> FrontendResult {
    let response = query_admin_modify_usertier(&app, bearer, &params).await?;

    Ok(response)
}

/// `GET /admin/imitate-login/:admin_address/:user_address` -- Being an admin, login as a user in read-only mode
///
/// - user_address that is to be logged in by
/// We assume that the admin has already logged in, and has a bearer token ...
#[debug_handler]
pub async fn admin_login_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    InsecureClientIp(ip): InsecureClientIp,
    Path(mut params): Path<HashMap<String, String>>,
) -> FrontendResult {
    // First check if the login is authorized
    login_is_authorized(&app, ip).await?;

    // create a message and save it in redis
    // TODO: how many seconds? get from config?

    // Same parameters as when someone logs in as a user
    let expire_seconds: usize = 20 * 60;
    let nonce = Ulid::new();
    let issued_at = OffsetDateTime::now_utc();
    let expiration_time = issued_at.add(Duration::new(expire_seconds as i64, 0));

    // The admin user is the one that basically logs in, on behalf of the user
    // This will generate a login id for the admin, which we will be caching ...
    // I suppose with this, the admin can be logged in to one session at a time
    // let (caller, _semaphore) = app.bearer_is_authorized(bearer_token).await?;

    // Finally, check if the user is an admin. If he is, return "true" as the third triplet.
    // TODO: consider wrapping the output in a struct, instead of a triplet
    // TODO: Could try to merge this into the above query ...
    // This query will fail if it's not the admin...

    // get the admin field ...
    let admin_address: Address = params
        .get("admin_address")
        .ok_or_else(|| FrontendErrorResponse::BadRequest("Unable to find admin_address key in request".to_string()))?
        .parse::<Address>()
        .map_err(|err| { FrontendErrorResponse::BadRequest("Unable to parse user_address as an Address".to_string()) })?;


    // Fetch the user_address parameter from the login string ... (as who we want to be logging in ...)
    let user_address: Vec<u8> = params
        .get("user_address")
        .ok_or_else(|| FrontendErrorResponse::BadRequest("Unable to find user_address key in request".to_string()))?
        .parse::<Address>()
        .map_err(|err| { FrontendErrorResponse::BadRequest("Unable to parse user_address as an Address".to_string(), ) })?
        .to_fixed_bytes().into();

    // We want to login to llamanodes.com
    let login_domain = app
        .config
        .login_domain
        .clone()
        .unwrap_or_else(|| "llamanodes.com".to_string());

    // Also there must basically be a token, that says that one admin logins _as a user_.
    // I'm not yet fully sure how to handle with that logic specifically ...
    // TODO: get most of these from the app config
    // TODO: Let's check again who the message needs to be signed by;
    // if the message does not have to be signed by the user, include the user ...
    let message = Message {
        // TODO: don't unwrap
        // TODO: accept a login_domain from the request?
        domain: login_domain.parse().unwrap(),
        // In the case of the admin, the admin needs to sign the message, so we include this logic ...
        address: admin_address.to_fixed_bytes(), // user_address.to_fixed_bytes(),
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
    let db_replica = app.db_replica().context("login requires a replica database")?;

    // Get the user that we want to imitate from the read-only database (their id ...)
    // TODO: Only get the id, not the whole user object ...
    let user = user::Entity::find()
        .filter(user::Column::Address.eq(user_address))
        .one(db_replica.conn())
        .await?
        .ok_or(FrontendErrorResponse::BadRequest("Could not find user in db".to_string()))?;

    let admin = user::Entity::find()
        .filter(user::Column::Address.eq(admin_address.encode()))
        .one(db_replica.conn())
        .await?
        .ok_or(FrontendErrorResponse::BadRequest("Could not find admin in db".to_string()))?;

    // Note that the admin is trying to log in as this user
    let trail = admin_trail::ActiveModel {
        caller: sea_orm::Set(admin.id),
        imitating_user: sea_orm::Set(Some(user.id)),
        endpoint: sea_orm::Set("admin_login_get".to_string()),
        payload: sea_orm::Set(format!("{:?}", params)),
        ..Default::default()
    };
    trail
        .save(&db_conn)
        .await
        .context("saving user's pending_login")?;

    // Can there be two login-sessions at the same time?
    // I supposed if the user logs in, the admin would be logged out and vice versa

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
        imitating_user: sea_orm::Set(Some(user.id))
    };

    user_pending_login
        .save(&db_conn)
        .await
        .context("saving an admin trail pre login")?;

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

/// `POST /admin/login` - Register or login by posting a signed "siwe" message
/// It is recommended to save the returned bearer token in a cookie.
/// The bearer token can be used to authenticate other requests, such as getting user user's tats or modifying the user's profile
#[debug_handler]
pub async fn admin_login_post(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    InsecureClientIp(ip): InsecureClientIp,
    Query(query): Query<PostLoginQuery>,
    Json(payload): Json<PostLogin>,
) -> FrontendResult {
    login_is_authorized(&app, ip).await?;

    // Check for the signed bytes ..
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

    // TODO: Here we will need to re-find the parameter where the admin wants to log-in as the user ...
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

    let db_conn = app
        .db_conn()
        .context("deleting expired pending logins requires a db")?;

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

    // TODO: Maybe add a context?
    let imitating_user_id = user_pending_login.imitating_user
        .context("getting address of the imitating user")?;

    // TODO: limit columns or load whole user?
    // TODO: Right now this loads the whole admin. I assume we might want to load the user though (?) figure this out as we go along...
    let admin = user::Entity::find()
        .filter(user::Column::Address.eq(our_msg.address.as_ref()))
        .one(db_replica.conn())
        .await?
        .context("getting admin address")?;

    let imitating_user = user::Entity::find()
        .filter(user::Column::Id.eq(imitating_user_id))
        .one(db_replica.conn())
        .await?
        .context("admin address was not found!")?;

    // Add a message that the admin has logged in
    // Note that the admin is trying to log in as this user
    let trail = admin_trail::ActiveModel {
        caller: sea_orm::Set(admin.id),
        imitating_user: sea_orm::Set(Some(imitating_user.id)),
        endpoint: sea_orm::Set("admin_login_post".to_string()),
        payload: sea_orm::Set(format!("{:?}", payload)),
        ..Default::default()
    };
    trail
        .save(&db_conn)
        .await
        .context("saving an admin trail post login")?;

    // I supposed we also get the rpc_key, whatever this is used for (?).
    // I think the RPC key should still belong to the admin though in this case ...

    // the user is already registered
    let admin_rpc_key = rpc_key::Entity::find()
        .filter(rpc_key::Column::UserId.eq(admin.id))
        .all(db_replica.conn())
        .await
        .context("failed loading user's key")?;

    // create a bearer token for the user.
    let user_bearer_token = UserBearerToken::default();

    // json response with everything in it
    // we could return just the bearer token, but I think they will always request api keys and the user profile
    let response_json = json!({
        "rpc_keys": admin_rpc_key
            .into_iter()
            .map(|uk| (uk.id, uk))
            .collect::<HashMap<_, _>>(),
        "bearer_token": user_bearer_token,
        "imitating_user": imitating_user,
        "admin_user": admin,
    });

    let response = (StatusCode::OK, Json(response_json)).into_response();

    // add bearer to the database

    // expire in 2 days, because this is more critical (and shouldn't need to be done so long!)
    let expires_at = Utc::now()
        .checked_add_signed(chrono::Duration::days(2))
        .unwrap();

    // TODO: Here, the bearer token should include a message
    // TODO: Above, make sure that the calling address is an admin!
    // TODO: Above, make sure that the signed is the admin (address field),
    // but then in this request, the admin can pick which user to sign up as
    let user_login = login::ActiveModel {
        id: sea_orm::NotSet,
        bearer_token: sea_orm::Set(user_bearer_token.uuid()),
        user_id: sea_orm::Set(imitating_user.id),  // Yes, this should be the user ... because the rest of the applications takes this item, from the initial user
        expires_at: sea_orm::Set(expires_at),
        read_only: sea_orm::Set(true)
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

// TODO: This is basically an exact copy of the user endpoint, I should probabl refactor this code ...
/// `POST /admin/imitate-logout` - Forget the bearer token in the `Authentication` header.
#[debug_handler]
pub async fn admin_logout_post(
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
