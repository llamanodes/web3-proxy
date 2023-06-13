//! Handle subusers, viewing subusers, and viewing accessible rpc-keys
use crate::app::Web3ProxyApp;
use crate::errors::{Web3ProxyError, Web3ProxyErrorContext, Web3ProxyResponse};
use crate::frontend::authorization::RpcSecretKey;
use anyhow::Context;
use axum::{
    extract::Query,
    headers::{authorization::Bearer, Authorization},
    response::IntoResponse,
    Extension, Json, TypedHeader,
};
use axum_macros::debug_handler;
use entities::sea_orm_active_enums::Role;
use entities::{balance, rpc_key, secondary_user, user};
use ethers::types::Address;
use hashbrown::HashMap;
use http::StatusCode;
use log::trace;
use migration::sea_orm;
use migration::sea_orm::ActiveModelTrait;
use migration::sea_orm::ColumnTrait;
use migration::sea_orm::EntityTrait;
use migration::sea_orm::IntoActiveModel;
use migration::sea_orm::QueryFilter;
use migration::sea_orm::TransactionTrait;
use serde_json::json;
use std::sync::Arc;
use ulid::{self, Ulid};

pub async fn get_keys_as_subuser(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
    Query(_params): Query<HashMap<String, String>>,
) -> Web3ProxyResponse {
    // First, authenticate
    let (subuser, _semaphore) = app.bearer_is_authorized(bearer).await?;

    let db_replica = app
        .db_replica()
        .context("getting replica db for user's revert logs")?;

    // TODO: JOIN over RPC_KEY, SUBUSER, PRIMARY_USER and return these items

    // Get all secondary users that have access to this rpc key
    let secondary_user_entities = secondary_user::Entity::find()
        .filter(secondary_user::Column::UserId.eq(subuser.id))
        .all(db_replica.as_ref())
        .await?
        .into_iter()
        .map(|x| (x.rpc_secret_key_id, x))
        .collect::<HashMap<u64, secondary_user::Model>>();

    // Now return a list of all subusers (their wallets)
    let rpc_key_entities: Vec<(rpc_key::Model, Option<user::Model>)> = rpc_key::Entity::find()
        .filter(
            rpc_key::Column::Id.is_in(
                secondary_user_entities
                    .iter()
                    .map(|(x, _)| *x)
                    .collect::<Vec<_>>(),
            ),
        )
        .find_also_related(user::Entity)
        .all(db_replica.as_ref())
        .await?;

    // TODO: Merge rpc-key with respective user (join is probably easiest ...)

    // Now return the list
    let response_json = json!({
        "subuser": format!("{:?}", Address::from_slice(&subuser.address)),
        "rpc_keys": rpc_key_entities
            .into_iter()
            .flat_map(|(rpc_key, rpc_owner)| {
                match rpc_owner {
                    Some(inner_rpc_owner) => {
                        let mut tmp = HashMap::new();
                        tmp.insert("rpc-key", serde_json::Value::String(Ulid::from(rpc_key.secret_key).to_string()));
                        tmp.insert("rpc-owner", serde_json::Value::String(format!("{:?}", Address::from_slice(&inner_rpc_owner.address))));
                        tmp.insert("role", serde_json::Value::String(format!("{:?}", secondary_user_entities.get(&rpc_key.id).unwrap().role))); // .to_string() returns ugly "'...'"
                        Some(tmp)
                    },
                    None => {
                        // error!("Found RPC secret key with no user!".to_owned());
                        None
                    }
                }
            })
            .collect::<Vec::<_>>(),
    });

    Ok(Json(response_json).into_response())
}

pub async fn get_subusers(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
    Query(mut params): Query<HashMap<String, String>>,
) -> Web3ProxyResponse {
    // First, authenticate
    let (user, _semaphore) = app.bearer_is_authorized(bearer).await?;

    let db_replica = app
        .db_replica()
        .context("getting replica db for user's revert logs")?;

    let rpc_key: u64 = params
        .remove("key_id")
        // TODO: map_err so this becomes a 500. routing must be bad
        .ok_or(Web3ProxyError::BadRequest(
            "You have not provided the 'rpc_key' whose access to modify".into(),
        ))?
        .parse()
        .context(format!("unable to parse key_id {:?}", params))?;

    // Get the rpc key id
    let rpc_key = rpc_key::Entity::find()
        .filter(rpc_key::Column::Id.eq(rpc_key))
        .one(db_replica.as_ref())
        .await?
        .ok_or(Web3ProxyError::BadRequest(
            "The provided RPC key cannot be found".into(),
        ))?;

    // Get all secondary users that have access to this rpc key
    let secondary_user_entities = secondary_user::Entity::find()
        .filter(secondary_user::Column::RpcSecretKeyId.eq(rpc_key.id))
        .all(db_replica.as_ref())
        .await?
        .into_iter()
        .map(|x| (x.user_id, x))
        .collect::<HashMap<u64, secondary_user::Model>>();

    // Now return a list of all subusers (their wallets)
    let subusers = user::Entity::find()
        .filter(
            user::Column::Id.is_in(
                secondary_user_entities
                    .iter()
                    .map(|(x, _)| *x)
                    .collect::<Vec<_>>(),
            ),
        )
        .all(db_replica.as_ref())
        .await?;

    trace!("Subusers are: {}", json!(subusers));

    // Now return the list
    let response_json = json!({
        "caller": format!("{:?}", Address::from_slice(&user.address)),
        "rpc_key": rpc_key,
        "subusers": subusers
            .into_iter()
            .map(|subuser| {
                let mut tmp = HashMap::new();
                // .encode_hex()
                tmp.insert("address", serde_json::Value::String(format!("{:?}", Address::from_slice(&subuser.address))));
                tmp.insert("role", serde_json::Value::String(format!("{:?}", secondary_user_entities.get(&subuser.id).unwrap().role)));
                json!(tmp)
            })
            .collect::<Vec::<_>>(),
    });

    Ok(Json(response_json).into_response())
}

#[debug_handler]
pub async fn modify_subuser(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
    Query(mut params): Query<HashMap<String, String>>,
) -> Web3ProxyResponse {
    // First, authenticate
    let (user, _semaphore) = app.bearer_is_authorized(bearer).await?;

    let db_replica = app
        .db_replica()
        .context("getting replica db for user's revert logs")?;

    trace!("Parameters are: {:?}", params);

    // Then, distinguish the endpoint to modify
    let rpc_key_to_modify: u64 = params
        .remove("key_id")
        // TODO: map_err so this becomes a 500. routing must be bad
        .ok_or(Web3ProxyError::BadRequest(
            "You have not provided the 'rpc_key' whose access to modify".into(),
        ))?
        .parse()
        .context(format!("unable to get the key_id {:?}", params))?;

    let subuser_address: Address = params
        .remove("subuser_address")
        // TODO: map_err so this becomes a 500. routing must be bad
        .ok_or(Web3ProxyError::BadRequest(
            "You have not provided the 'user_address' whose access to modify".into(),
        ))?
        .parse()
        .context(format!("unable to parse subuser_address {:?}", params))?;

    // TODO: Check subuser address for eip55 checksum

    let keep_subuser: bool = match params
        .remove("new_status")
        // TODO: map_err so this becomes a 500. routing must be bad
        .ok_or(Web3ProxyError::BadRequest(
            "You have not provided the new_stats key in the request".into(),
        ))?
        .as_str()
    {
        "upsert" => Ok(true),
        "remove" => Ok(false),
        _ => Err(Web3ProxyError::BadRequest(
            "'new_status' must be one of 'upsert' or 'remove'".into(),
        )),
    }?;

    let new_role: Role = match params
        .remove("new_role")
        // TODO: map_err so this becomes a 500. routing must be bad
        .ok_or(Web3ProxyError::BadRequest(
            "You have not provided the new_role key in the request".into(),
        ))?
        .as_str()
    {
        // TODO: Technically, if this is the new owner, we should transpose the full table.
        // For now, let's just not allow the primary owner to just delete his account
        // (if there is even such a functionality)
        "owner" => Ok(Role::Owner),
        "admin" => Ok(Role::Admin),
        "collaborator" => Ok(Role::Collaborator),
        _ => Err(Web3ProxyError::BadRequest(
            "'new_role' must be one of 'owner', 'admin', 'collaborator'".into(),
        )),
    }?;

    // ---------------------------
    // First, check if the user exists as a user. If not, add them
    // (and also create a balance, and rpc_key, same procedure as logging in for first time)
    // ---------------------------
    let subuser = user::Entity::find()
        .filter(user::Column::Address.eq(subuser_address.as_ref()))
        .one(db_replica.as_ref())
        .await?;

    let rpc_key_entity = rpc_key::Entity::find()
        .filter(rpc_key::Column::Id.eq(rpc_key_to_modify))
        .one(db_replica.as_ref())
        .await?
        .ok_or(Web3ProxyError::BadRequest(
            "Provided RPC key does not exist!".into(),
        ))?;

    // Make sure that the user owns the rpc_key_entity
    if rpc_key_entity.user_id != user.id {
        return Err(Web3ProxyError::BadRequest(
            "you must own the RPC for which you are giving permissions out".into(),
        ));
    }

    // TODO: There is a good chunk of duplicate logic as login-post. Consider refactoring ...
    let db_conn = app.db_conn().web3_context("login requires a db")?;
    let (subuser, _subuser_rpc_keys, _status_code) = match subuser {
        None => {
            let txn = db_conn.begin().await?;
            // First add a user; the only thing we need from them is an address
            // everything else is optional
            let subuser = user::ActiveModel {
                address: sea_orm::Set(subuser_address.to_fixed_bytes().into()), // Address::from_slice(
                ..Default::default()
            };

            let subuser = subuser.insert(&txn).await?;

            // create the user's first api key
            let rpc_secret_key = RpcSecretKey::new();

            let subuser_rpc_key = rpc_key::ActiveModel {
                user_id: sea_orm::Set(subuser.id),
                secret_key: sea_orm::Set(rpc_secret_key.into()),
                description: sea_orm::Set(None),
                ..Default::default()
            };

            let subuser_rpc_keys = vec![subuser_rpc_key
                .insert(&txn)
                .await
                .web3_context("Failed saving new user key")?];

            // We should also create the balance entry ...
            let subuser_balance = balance::ActiveModel {
                user_id: sea_orm::Set(subuser.id),
                ..Default::default()
            };
            subuser_balance.insert(&txn).await?;
            // save the user and key to the database
            txn.commit().await?;

            (subuser, subuser_rpc_keys, StatusCode::CREATED)
        }
        Some(subuser) => {
            if subuser.id == user.id {
                return Err(Web3ProxyError::BadRequest(
                    "you cannot make a subuser out of yourself".into(),
                ));
            }

            // Let's say that a user that exists can actually also redeem a key in retrospect...
            // the user is already registered
            let subuser_rpc_keys = rpc_key::Entity::find()
                .filter(rpc_key::Column::UserId.eq(subuser.id))
                .all(db_replica.as_ref())
                .await
                .web3_context("failed loading user's key")?;

            (subuser, subuser_rpc_keys, StatusCode::OK)
        }
    };

    // --------------------------------
    // Now apply the operation
    // Either add the subuser
    // Or revoke his subuser status
    // --------------------------------

    // Search for subuser first of all
    // There should be a unique-constraint on user-id + rpc_key
    let subuser_entry_secondary_user = secondary_user::Entity::find()
        .filter(secondary_user::Column::UserId.eq(subuser.id))
        .filter(secondary_user::Column::RpcSecretKeyId.eq(rpc_key_entity.id))
        .one(db_replica.as_ref())
        .await
        .web3_context("failed using the db to check for a subuser")?;

    let txn = db_conn.begin().await?;
    let mut action = "no action";

    match subuser_entry_secondary_user {
        Some(secondary_user) => {
            // In this case, remove the subuser
            let mut active_subuser_entry_secondary_user = secondary_user.into_active_model();
            if !keep_subuser {
                // Remove the user
                active_subuser_entry_secondary_user.delete(&db_conn).await?;
                action = "removed";
            } else {
                // Just change the role
                active_subuser_entry_secondary_user.role = sea_orm::Set(new_role.clone());
                active_subuser_entry_secondary_user.save(&db_conn).await?;
                action = "role modified";
            }
        }
        None if keep_subuser => {
            let active_subuser_entry_secondary_user = secondary_user::ActiveModel {
                user_id: sea_orm::Set(subuser.id),
                rpc_secret_key_id: sea_orm::Set(rpc_key_entity.id),
                role: sea_orm::Set(new_role.clone()),
                ..Default::default()
            };
            active_subuser_entry_secondary_user.insert(&txn).await?;
            action = "added";
        }
        _ => {
            // Return if the user should be removed and if there is no entry;
            // in this case, the user is not entered

            // Return if the user should be added and there is already an entry;
            // in this case, they were already added, so we can skip this
            // Do nothing in this case
        }
    };
    txn.commit().await?;

    let response = (
        StatusCode::OK,
        Json(json!({
            "rpc_key": rpc_key_to_modify,
            "subuser_address": subuser_address,
            "keep_user": keep_subuser,
            "new_role": new_role,
            "action": action
        })),
    )
        .into_response();

    // Return early if the log was added, assume there is at most one valid log per transaction
    Ok(response)
}
