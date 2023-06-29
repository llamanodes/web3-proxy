use crate::app::Web3ProxyApp;
use crate::errors::{Web3ProxyError, Web3ProxyResponse};
use crate::http_params::get_user_id_from_params;
use axum::response::IntoResponse;
use axum::{
    headers::{authorization::Bearer, Authorization},
    Json, TypedHeader,
};
use entities::{admin, login, user, user_tier};
use ethers::prelude::Address;
use hashbrown::HashMap;
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter,
};
use tracing::{info, trace};

// TODO: Add some logic to check if the operating user is an admin
// If he is, return true
// If he is not, return false
// This function is used to give permission to certain users

pub async fn query_admin_modify_usertier<'a>(
    app: &'a Web3ProxyApp,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    params: &'a HashMap<String, String>,
) -> Web3ProxyResponse {
    // Quickly return if any of the input tokens are bad
    let user_address = params
        .get("user_address")
        .ok_or_else(|| {
            Web3ProxyError::BadRequest("Unable to find user_address key in request".into())
        })?
        .parse::<Address>()
        .map_err(|_| {
            Web3ProxyError::BadRequest("Unable to parse user_address as an Address".into())
        })?;
    let user_tier_title = params.get("user_tier_title").ok_or_else(|| {
        Web3ProxyError::BadRequest("Unable to get the user_tier_title key from the request".into())
    })?;

    // Prepare output body
    let mut response_body = HashMap::new();

    // Establish connections
    let db_conn = app.db_conn()?;
    let db_replica = app.db_replica()?;
    let mut redis_conn = app.redis_conn().await?;

    // Will modify logic here

    // Try to get the user who is calling from redis (if existent) / else from the database
    // TODO: Make a single query, where you retrieve the user, and directly from it the secondary user (otherwise we do two jumpy, which is unnecessary)
    // get the user id first. if it is 0, we should use a cache on the app
    let caller_id =
        get_user_id_from_params(&mut redis_conn, db_conn, db_replica, bearer, params).await?;

    trace!(%caller_id, "query_admin_modify_usertier");

    // Check if the caller is an admin (i.e. if he is in an admin table)
    let _admin = admin::Entity::find()
        .filter(admin::Column::UserId.eq(caller_id))
        .one(db_conn)
        .await?
        .ok_or(Web3ProxyError::AccessDenied("not an admin".into()))?;

    // If we are here, that means an admin was found, and we can safely proceed

    // Fetch the admin, and the user
    let user: user::Model = user::Entity::find()
        .filter(user::Column::Address.eq(user_address.as_bytes()))
        .one(db_conn)
        .await?
        .ok_or(Web3ProxyError::BadRequest(
            "No user with this id found".into(),
        ))?;
    // Return early if the target user_tier_id is the same as the original user_tier_id
    response_body.insert(
        "user_tier_title",
        serde_json::Value::Number(user.user_tier_id.into()),
    );

    // Now we can modify the user's tier
    let new_user_tier: user_tier::Model = user_tier::Entity::find()
        .filter(user_tier::Column::Title.eq(user_tier_title.clone()))
        .one(db_conn)
        .await?
        .ok_or(Web3ProxyError::BadRequest(
            "User Tier name was not found".into(),
        ))?;

    if user.user_tier_id == new_user_tier.id {
        info!("user already has that tier");
    } else {
        let mut user = user.clone().into_active_model();

        user.user_tier_id = sea_orm::Set(new_user_tier.id);

        user.save(db_conn).await?;

        info!("user's tier changed");
    }

    // Now delete all bearer tokens of this user
    login::Entity::delete_many()
        .filter(login::Column::UserId.eq(user.id))
        .exec(db_conn)
        .await?;

    Ok(Json(&response_body).into_response())
}
