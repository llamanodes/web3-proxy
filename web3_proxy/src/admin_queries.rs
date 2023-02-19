use crate::app::Web3ProxyApp;
use crate::frontend::errors::FrontendErrorResponse;
use crate::user_queries::get_user_id_from_params;
use anyhow::Context;
use axum::{
    Json,
    headers::{authorization::Bearer, Authorization},
    TypedHeader,
};
use axum::response::{IntoResponse, Response};
use entities::{admin, user, user_tier};
use ethers::prelude::Address;
use hashbrown::HashMap;
use http::StatusCode;
use migration::sea_orm::{self, IntoActiveModel};
use log::info;


pub async fn query_admin_modify_usertier<'a>(
    app: &'a Web3ProxyApp,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    params: &'a HashMap<String, String>
) -> Result<Response, FrontendErrorResponse> {

    // Quickly return if any of the input tokens are bad
    let user_address: Vec<u8> = params
        .get("user_address")
        .ok_or_else(||
            FrontendErrorResponse::StatusCode(
                StatusCode::BAD_REQUEST,
                "Unable to find user_address key in request".to_string(),
                None,
            )
        )?
        .parse::<Address>()
        .map_err(|err| {
            FrontendErrorResponse::StatusCode(
                StatusCode::BAD_REQUEST,
                "Unable to parse user_address as an Address".to_string(),
                Some(err.into()),
            )
        })?
        .to_fixed_bytes().into();
    let user_tier_title = params
        .get("user_tier_title")
        .ok_or_else(|| FrontendErrorResponse::StatusCode(
            StatusCode::BAD_REQUEST,
            "Unable to get the user_tier_title key from the request".to_string(),
            None,
        ))?;

    // Prepare output body
    let mut response_body = HashMap::new();
    response_body.insert(
        "user_address",
        serde_json::Value::String(user_address.into()),
    );
    response_body.insert(
        "user_tier_title",
        serde_json::Value::String(user_tier_title.into()),
    );

    // Establish connections
    let db_conn = app.db_conn().context("query_admin_modify_user needs a db")?;
    let db_replica = app
        .db_replica()
        .context("query_user_stats needs a db replica")?;
    let mut redis_conn = app
        .redis_conn()
        .await
        .context("query_admin_modify_user had a redis connection error")?
        .context("query_admin_modify_user needs a redis")?;

    // Try to get the user who is calling from redis (if existent) / else from the database
    // TODO: Make a single query, where you retrieve the user, and directly from it the secondary user (otherwise we do two jumpy, which is unnecessary)
    // get the user id first. if it is 0, we should use a cache on the app
    let caller_id = get_user_id_from_params(&mut redis_conn, &db_conn, &db_replica, bearer, &params).await?;

    // Check if the caller is an admin (i.e. if he is in an admin table)
    let admin: admin::Model = admin::Entity::find()
        .filter(admin::Entity::UserId.eq(caller_id))
        .one(&db_replica)
        .await?
        .context("This user is not registered as an admin")?;

    // If we are here, that means an admin was found, and we can safely proceed

    // Fetch the admin, and the user
    let user: user::Model = user::Entity::find()
        .filter(user::Column::Address.eq(user_address))
        .one(&db_replica)
        .await?
        .context("No user with this id found as the change")?;
    // Return early if the target user_tier_id is the same as the original user_tier_id
    response_body.insert(
        "user_tier_title",
        serde_json::Value::String(user.user_tier_id.into()),
    );

    // Now we can modify the user's tier
    let new_user_tier: user_tier::Model = user_tier::Entity::find()
        .filter(user_tier::Column::Title.eq(user_tier_title.clone()))
        .one(&db_replica)
        .await?
        .context("No user tier found with that name")?;

    if user.user_tier_id == new_user_tier.id {
        info!("user already has that tier");
    } else {
        let mut user = user.into_active_model();

        user.user_tier_id = sea_orm::Set(new_user_tier.id);

        user.save(&db_conn).await?;

        info!("user's tier changed");
    }

    // Finally, remove the user from redis
    // TODO: Also remove the user from the redis

    Ok(Json(&response_body).into_response())

}
