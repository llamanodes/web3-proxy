//! Handle admin helper logic

use super::authorization::login_is_authorized;
use crate::admin_queries::query_admin_modify_usertier;
use crate::app::Web3ProxyApp;
use crate::errors::Web3ProxyResponse;
use crate::errors::{Web3ProxyError, Web3ProxyErrorContext};
use crate::frontend::users::authentication::PostLogin;
use crate::user_token::UserBearerToken;
use axum::{
    extract::{Path, Query},
    headers::{authorization::Bearer, Authorization},
    response::IntoResponse,
    Extension, Json, TypedHeader,
};
use axum_client_ip::InsecureClientIp;
use axum_macros::debug_handler;
use chrono::{TimeZone, Utc};
use entities::{
    admin, admin_increase_balance_receipt, admin_trail, login, pending_login, rpc_key,
    user,
};
use ethers::{prelude::Address, types::Bytes};
use hashbrown::HashMap;
use http::StatusCode;
use migration::sea_orm::prelude::{Decimal, Uuid};
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use siwe::{Message, VerificationOpts};
use std::ops::Add;
use std::str::FromStr;
use std::sync::Arc;
use time_03::{Duration, OffsetDateTime};
use tracing::{info, trace, warn};
use ulid::Ulid;

#[derive(Debug, Deserialize, Serialize)]
pub struct AdminIncreaseBalancePost {
    pub user_address: Address,
    pub note: Option<String>,
    pub amount: Decimal,
}

/// `POST /admin/increase_balance` -- As an admin, modify a user's user-tier
///
/// - user_address that is to credited balance
/// - user_role_tier that is supposed to be adapted
#[debug_handler]
pub async fn admin_increase_balance(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
    Json(payload): Json<AdminIncreaseBalancePost>,
) -> Web3ProxyResponse {
    let caller = app.bearer_is_authorized(bearer).await?;

    // Establish connections
    let txn = app.db_transaction().await?;

    // Check if the caller is an admin (if not, return early)
    let admin_entry: admin::Model = admin::Entity::find()
        .filter(admin::Column::UserId.eq(caller.id))
        .one(&txn)
        .await?
        .ok_or_else(|| Web3ProxyError::AccessDenied("not an admin".into()))?;

    let user_entry: user::Model = user::Entity::find()
        .filter(user::Column::Address.eq(payload.user_address.as_bytes()))
        .one(&txn)
        .await?
        .ok_or(Web3ProxyError::BadRequest(
            format!("No user found with {:?}", payload.user_address).into(),
        ))?;

    let increase_balance_receipt = admin_increase_balance_receipt::ActiveModel {
        amount: sea_orm::Set(payload.amount),
        admin_id: sea_orm::Set(admin_entry.id),
        deposit_to_user_id: sea_orm::Set(user_entry.id),
        note: sea_orm::Set(payload.note.unwrap_or_default()),
        ..Default::default()
    };
    increase_balance_receipt.save(&txn).await?;
    txn.commit().await?;

    // Invalidate the user_balance_cache for this user:
    app.user_balance_cache.invalidate(&user_entry.id).await;

    let out = json!({
        "user": payload.user_address,
        "amount": payload.amount,
    });

    Ok(Json(out).into_response())
}

/// `POST /admin/modify_role` -- As an admin, modify a user's user-tier
///
/// - user_address that is to be modified
/// - user_role_tier that is supposed to be adapted
///
/// TODO: JSON post data instead of query params
#[debug_handler]
pub async fn admin_change_user_roles(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    Query(params): Query<HashMap<String, String>>,
) -> Web3ProxyResponse {
    let response = query_admin_modify_usertier(&app, bearer, &params).await?;

    Ok(response)
}

/// `GET /admin/imitate-login/:admin_address/:user_address` -- Being an admin, login as a user in read-only mode
///
/// - user_address that is to be logged in by
/// We assume that the admin has already logged in, and has a bearer token ...
#[debug_handler]
pub async fn admin_imitate_login_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    InsecureClientIp(ip): InsecureClientIp,
    Path(mut params): Path<HashMap<String, String>>,
) -> Web3ProxyResponse {
    // First check if the login is authorized
    login_is_authorized(&app, ip).await?;

    // create a message and save it in redis
    // TODO: how many seconds? get from config?

    // Same parameters as when someone logs in as a user
    let expire_seconds: usize = 20 * 60;
    let nonce = Ulid::new();
    let issued_at = OffsetDateTime::now_utc();
    let expiration_time = issued_at.add(Duration::new(expire_seconds as i64, 0));

    // get the admin's address
    let admin_address: Address = params
        .get("admin_address")
        .ok_or_else(|| {
            Web3ProxyError::BadRequest("Unable to find admin_address key in request".into())
        })?
        .parse::<Address>()
        .map_err(|_err| {
            Web3ProxyError::BadRequest("Unable to parse admin_address as an Address".into())
        })?;

    // get the address who we want to be logging in as
    let user_address: Address = params
        .get("user_address")
        .ok_or_else(|| {
            Web3ProxyError::BadRequest("Unable to find user_address key in request".into())
        })?
        .parse::<Address>()
        .map_err(|_err| {
            Web3ProxyError::BadRequest("Unable to parse user_address as an Address".into())
        })?;

    // We want to login to llamanodes.com
    let domain = app
        .config
        .login_domain
        .as_deref()
        .unwrap_or("llamanodes.com");

    let message_domain = domain.parse()?;
    // TODO: don't unwrap
    let message_uri = format!("https://{}/", domain).parse().unwrap();

    // TODO: get most of these from the app config
    let message = Message {
        domain: message_domain,
        // the admin needs to sign the message, not the imitated user
        address: admin_address.to_fixed_bytes(),
        // TODO: config for statement
        statement: Some("👑👑👑👑👑".to_string()),
        uri: message_uri,
        version: siwe::Version::V1,
        chain_id: app.config.chain_id,
        expiration_time: Some(expiration_time.into()),
        issued_at: issued_at.into(),
        nonce: nonce.to_string(),
        not_before: None,
        request_id: None,
        resources: vec![],
    };

    let db_conn = app.db_conn()?;
    let db_replica = app.db_replica()?;

    let admin = user::Entity::find()
        .filter(user::Column::Address.eq(admin_address.as_bytes()))
        .one(db_replica.as_ref())
        .await?
        .ok_or(Web3ProxyError::AccessDenied("not an admin".into()))?;

    // Get the user that we want to imitate from the read-only database (their id ...)
    // TODO: Only get the id, not the whole user object ...
    let user = user::Entity::find()
        .filter(user::Column::Address.eq(user_address.as_bytes()))
        .one(db_replica.as_ref())
        .await?
        .ok_or(Web3ProxyError::BadRequest(
            "Could not find user in db".into(),
        ))?;

    info!(admin=?admin.address, user=?user.address, "admin is imitating another user");

    // delete ALL expired rows.
    let now = Utc::now();
    let delete_result = pending_login::Entity::delete_many()
        .filter(pending_login::Column::ExpiresAt.lte(now))
        .exec(db_conn)
        .await?;
    trace!("cleared expired pending_logins: {:?}", delete_result);

    // Note that the admin is trying to log in as this user
    let trail = admin_trail::ActiveModel {
        caller: sea_orm::Set(admin.id),
        imitating_user: sea_orm::Set(Some(user.id)),
        endpoint: sea_orm::Set("admin_imitate_login_get".to_string()),
        payload: sea_orm::Set(format!("{}", json!(params))),
        ..Default::default()
    };

    trail
        .save(db_conn)
        .await
        .web3_context("saving user's pending_login")?;

    // Can there be two login-sessions at the same time?
    // I supposed if the user logs in, the admin would be logged out and vice versa

    // we add 1 to expire_seconds just to be sure the database has the key for the full expiration_time
    let expires_at = Utc
        .timestamp_opt(expiration_time.unix_timestamp() + 1, 0)
        .unwrap();

    // we do not store a maximum number of attempted logins. anyone can request so we don't want to allow DOS attacks
    // add a row to the database for this user
    let user_pending_login = pending_login::ActiveModel {
        id: sea_orm::NotSet,
        nonce: sea_orm::Set(nonce.into()),
        message: sea_orm::Set(message.to_string()),
        expires_at: sea_orm::Set(expires_at),
        imitating_user: sea_orm::Set(Some(user.id)),
    };

    user_pending_login
        .save(db_conn)
        .await
        .web3_context("saving an admin trail pre login")?;

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
            return Err(Web3ProxyError::InvalidEip);
        }
    };

    Ok(message.into_response())
}

/// `POST /admin/imitate-login` - Admin login by posting a signed "siwe" message
/// It is recommended to save the returned bearer token in a cookie.
/// The bearer token can be used to authenticate other admin requests
#[debug_handler]
pub async fn admin_imitate_login_post(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    InsecureClientIp(ip): InsecureClientIp,
    Json(payload): Json<PostLogin>,
) -> Web3ProxyResponse {
    login_is_authorized(&app, ip).await?;

    // Check for the signed bytes ..
    // TODO: this seems too verbose. how can we simply convert a String into a [u8; 65]
    let their_sig_bytes = Bytes::from_str(&payload.sig).web3_context("parsing sig")?;
    if their_sig_bytes.len() != 65 {
        return Err(Web3ProxyError::InvalidSignatureLength);
    }
    let mut their_sig: [u8; 65] = [0; 65];
    for x in 0..65 {
        their_sig[x] = their_sig_bytes[x]
    }

    // we can't trust that they didn't tamper with the message in some way. like some clients return it hex encoded
    // TODO: checking 0x seems fragile, but I think it will be fine. siwe message text shouldn't ever start with 0x
    let their_msg: Message = if payload.msg.starts_with("0x") {
        let their_msg_bytes = Bytes::from_str(&payload.msg).map_err(|err| {
            Web3ProxyError::BadRequest(
                format!("error parsing payload message as Bytes: {}", err).into(),
            )
        })?;

        // TODO: lossy or no?
        String::from_utf8_lossy(their_msg_bytes.as_ref())
            .parse::<siwe::Message>()
            .map_err(|err| {
                Web3ProxyError::BadRequest(
                    format!("error parsing bytes as siwe message: {}", err).into(),
                )
            })?
    } else {
        payload.msg.parse::<siwe::Message>().map_err(|err| {
            Web3ProxyError::BadRequest(
                format!("error parsing string as siwe message: {}", err).into(),
            )
        })?
    };

    // the only part of the message we will trust is their nonce
    // TODO: this is fragile. have a helper function/struct for redis keys
    let login_nonce = UserBearerToken::from_str(&their_msg.nonce).map_err(|err| {
        Web3ProxyError::BadRequest(format!("error parsing nonce: {}", err).into())
    })?;

    // fetch the message we gave them from our database
    let db_replica = app.db_replica()?;

    let user_pending_login = pending_login::Entity::find()
        .filter(pending_login::Column::Nonce.eq(Uuid::from(login_nonce.clone())))
        .one(db_replica.as_ref())
        .await
        .web3_context("database error while finding pending_login")?
        .web3_context("login nonce not found")?;

    let our_msg: siwe::Message = user_pending_login
        .message
        .parse()
        .web3_context("parsing siwe message")?;

    // mostly default options are fine. the message includes timestamp and domain and nonce
    let verify_config = VerificationOpts {
        rpc_provider: Some(app.internal_provider().clone()),
        ..Default::default()
    };

    our_msg
        .verify(&their_sig, &verify_config)
        .await
        .web3_context("verifying signature against our local message")?;

    let imitating_user_id = user_pending_login
        .imitating_user
        .web3_context("getting address of the imitating user")?;

    // TODO: limit columns or load whole user?
    // TODO: Right now this loads the whole admin. I assume we might want to load the user though (?) figure this out as we go along...
    let admin = user::Entity::find()
        .filter(user::Column::Address.eq(our_msg.address.as_ref()))
        .one(db_replica.as_ref())
        .await?
        .web3_context("getting admin address")?;

    let imitating_user = user::Entity::find()
        .filter(user::Column::Id.eq(imitating_user_id))
        .one(db_replica.as_ref())
        .await?
        .web3_context("admin address was not found!")?;

    let db_conn = app.db_conn()?;

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
        .save(db_conn)
        .await
        .web3_context("saving an admin trail post login")?;

    // I supposed we also get the rpc_key, whatever this is used for (?).
    // I think the RPC key should still belong to the admin though in this case ...

    // the user is already registered
    let admin_rpc_key = rpc_key::Entity::find()
        .filter(rpc_key::Column::UserId.eq(admin.id))
        .all(db_replica.as_ref())
        .await
        .web3_context("failed loading user's key")?;

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
    let expires_at = Utc::now() + chrono::Duration::days(2);

    // TODO: Here, the bearer token should include a message
    // TODO: Above, make sure that the calling address is an admin!
    // TODO: Above, make sure that the signed is the admin (address field),
    // but then in this request, the admin can pick which user to sign up as
    let user_login = login::ActiveModel {
        id: sea_orm::NotSet,
        bearer_token: sea_orm::Set(user_bearer_token.uuid()),
        user_id: sea_orm::Set(imitating_user.id), // Yes, this should be the user ... because the rest of the applications takes this item, from the initial user
        expires_at: sea_orm::Set(expires_at),
        read_only: sea_orm::Set(true),
    };

    user_login
        .save(db_conn)
        .await
        .web3_context("saving user login")?;

    if let Err(err) = user_pending_login.into_active_model().delete(db_conn).await {
        warn!(none=?login_nonce.0, ?err, "Failed to delete nonce");
    }

    Ok(response)
}
