//! Handle registration, logins, and managing account data.
use crate::app::Web3ProxyApp;
use crate::frontend::errors::Web3ProxyResponse;
use crate::referral_code::ReferralCode;
use anyhow::Context;
use axum::{
    extract::Query,
    headers::{authorization::Bearer, Authorization},
    response::IntoResponse,
    Extension, Json, TypedHeader,
};
use axum_macros::debug_handler;
use entities::referrer;
use hashbrown::HashMap;
use http::StatusCode;
use migration::sea_orm;
use migration::sea_orm::ActiveModelTrait;
use migration::sea_orm::ColumnTrait;
use migration::sea_orm::EntityTrait;
use migration::sea_orm::QueryFilter;
use serde_json::json;
use std::sync::Arc;

/// Create or get the existing referral link.
/// This is the link that the user can share to third parties, and get credits.
/// Applies to premium users only
#[debug_handler]
pub async fn user_referral_link_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
    Query(_params): Query<HashMap<String, String>>,
) -> Web3ProxyResponse {
    // First get the bearer token and check if the user is logged in
    let (user, _semaphore) = app.bearer_is_authorized(bearer).await?;

    let db_replica = app
        .db_replica()
        .context("getting replica db for user's revert logs")?;

    // Then get the referral token. If one doesn't exist, create one
    let user_referrer = referrer::Entity::find()
        .filter(referrer::Column::UserId.eq(user.id))
        .one(db_replica.conn())
        .await?;

    let (referral_code, status_code) = match user_referrer {
        Some(x) => (x.referral_code, StatusCode::OK),
        None => {
            // Connect to the database for writes
            let db_conn = app.db_conn().context("getting db_conn")?;

            let referral_code = ReferralCode::default().to_string();

            let referrer_entry = referrer::ActiveModel {
                user_id: sea_orm::ActiveValue::Set(user.id),
                referral_code: sea_orm::ActiveValue::Set(referral_code.clone()),
                ..Default::default()
            };
            referrer_entry.save(&db_conn).await?;

            (referral_code, StatusCode::CREATED)
        }
    };

    let response_json = json!({
        "referral_code": referral_code,
        "user": user,
    });

    let response = (status_code, Json(response_json)).into_response();
    Ok(response)
}
