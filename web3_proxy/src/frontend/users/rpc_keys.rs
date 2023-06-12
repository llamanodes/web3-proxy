//! Handle registration, logins, and managing account data.
use super::super::authorization::RpcSecretKey;
use crate::app::Web3ProxyApp;
use crate::errors::{Web3ProxyError, Web3ProxyErrorContext, Web3ProxyResponse};
use axum::headers::{Header, Origin, Referer, UserAgent};
use axum::{
    headers::{authorization::Bearer, Authorization},
    response::IntoResponse,
    Extension, Json, TypedHeader,
};
use axum_macros::debug_handler;
use entities;
use entities::sea_orm_active_enums::{Role, TrackingLevel};
use entities::{rpc_key, secondary_user};
use hashbrown::HashMap;
use http::HeaderValue;
use ipnet::IpNet;
use itertools::Itertools;
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter, TryIntoModel,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// `GET /user/keys` -- Use a bearer token to get the user's api keys and their settings.
#[debug_handler]
pub async fn rpc_keys_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
) -> Web3ProxyResponse {
    let (user, _semaphore) = app.bearer_is_authorized(bearer).await?;

    let db_replica = app
        .db_replica()
        .web3_context("db_replica is required to fetch a user's keys")?;

    let uks = rpc_key::Entity::find()
        .filter(rpc_key::Column::UserId.eq(user.id))
        .all(db_replica.as_ref())
        .await
        .web3_context("failed loading user's key")?;

    let secondary_user_entities = secondary_user::Entity::find()
        .filter(secondary_user::Column::UserId.eq(user.id))
        .all(db_replica.as_ref())
        .await?
        .into_iter()
        .map(|x| (x.rpc_secret_key_id, x))
        .collect::<HashMap<u64, secondary_user::Model>>();

    // Now return a list of all subusers (their wallets)
    let rpc_key_entities: Vec<rpc_key::Model> = rpc_key::Entity::find()
        .filter(
            rpc_key::Column::Id.is_in(
                secondary_user_entities
                    .iter()
                    .map(|(x, _)| *x)
                    .collect::<Vec<_>>(),
            ),
        )
        .all(db_replica.as_ref())
        .await?;

    let response_json = json!({
        "user_id": user.id,
        "user_rpc_keys": uks
            .into_iter()
            .map(|uk| (uk.id, uk))
            .chain(rpc_key_entities.into_iter().map(|sk| (sk.id, sk)))
            .collect::<HashMap::<_, _>>(),
    });

    Ok(Json(response_json).into_response())
}

/// `DELETE /user/keys` -- Use a bearer token to delete an existing key.
#[debug_handler]
pub async fn rpc_keys_delete(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
) -> Web3ProxyResponse {
    let (_user, _semaphore) = app.bearer_is_authorized(bearer).await?;

    // TODO: think about how cascading deletes and billing should work
    Err(Web3ProxyError::NotImplemented)
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
    log_level: Option<TrackingLevel>,
    // TODO: enable log_revert_trace: Option<f64>,
    private_txs: Option<bool>,
}

/// `POST /user/keys` or `PUT /user/keys` -- Use a bearer token to create or update an existing key.
#[debug_handler]
pub async fn rpc_keys_management(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
    Json(payload): Json<UserKeyManagement>,
) -> Web3ProxyResponse {
    // TODO: is there a way we can know if this is a PUT or POST? right now we can modify or create keys with either. though that probably doesn't matter

    let (user, _semaphore) = app.bearer_is_authorized(bearer).await?;

    let db_replica = app
        .db_replica()
        .web3_context("getting db for user's keys")?;

    let mut uk = match payload.key_id {
        Some(existing_key_id) => {
            if let Some(x) = rpc_key::Entity::find()
                .filter(rpc_key::Column::UserId.eq(user.id))
                .filter(rpc_key::Column::Id.eq(existing_key_id))
                .one(db_replica.as_ref())
                .await
                .web3_context("failed loading user's key")?
            {
                Ok(x.into_active_model())
            } else {
                // Return early if there is no permissions; otherwise all the code below can work
                // (1) Check if the key is in the user's control, return early accordingly
                match secondary_user::Entity::find()
                    .filter(secondary_user::Column::UserId.eq(user.id))
                    .filter(secondary_user::Column::RpcSecretKeyId.eq(existing_key_id))
                    .find_also_related(rpc_key::Entity)
                    .one(db_replica.as_ref())
                    .await?
                {
                    // Match statement here, check in the user's RPC keys directly if it's not part of the secondary user
                    Some((secondary_user_entity, Some(rpc_key))) => {
                        // Check if the secondary user is an admin, return early if not
                        if secondary_user_entity.role == Role::Owner
                            || secondary_user_entity.role == Role::Admin
                        {
                            Ok(rpc_key.into_active_model())
                        } else {
                            Err(Web3ProxyError::AccessDenied)
                        }
                    }
                    Some((x, None)) => Err(Web3ProxyError::BadResponse(
                        "a subuser record was found, but no corresponding RPC key".into(),
                    )),
                    // Match statement here, check in the user's RPC keys directly if it's not part of the secondary user
                    None => {
                        // get the key and make sure it belongs to the user
                        Err(Web3ProxyError::BadRequest(
                            "key does not exist or is not controlled by this bearer token".into(),
                        ))
                    }
                }
            }
        }
        None => {
            // make a new key
            // TODO: limit to 10 keys?
            let secret_key = RpcSecretKey::new();

            let log_level = payload
                .log_level
                .web3_context("log level must be 'none', 'detailed', or 'aggregated'")?;

            Ok(rpc_key::ActiveModel {
                user_id: sea_orm::Set(user.id),
                secret_key: sea_orm::Set(secret_key.into()),
                log_level: sea_orm::Set(log_level),
                ..Default::default()
            })
        }
    }?;

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
        let db_conn = app.db_conn().web3_context("login requires a db")?;

        uk.save(&db_conn)
            .await
            .web3_context("Failed saving user key")?
    } else {
        uk
    };

    let uk = uk.try_into_model()?;

    Ok(Json(uk).into_response())
}
