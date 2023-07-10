//! Handle registration, logins, and managing account data.
use crate::app::Web3ProxyApp;
use crate::errors::Web3ProxyResponse;
use crate::referral_code::ReferralCode;
use anyhow::Context;
use axum::{
    extract::Query,
    headers::{authorization::Bearer, Authorization},
    response::IntoResponse,
    Extension, Json, TypedHeader,
};
use axum_macros::debug_handler;
use entities::{referee, referrer, user};
use ethers::types::Address;
use hashbrown::HashMap;
use http::StatusCode;
use migration::sea_orm;
use migration::sea_orm::prelude::{DateTime, Decimal};
use migration::sea_orm::ActiveModelTrait;
use migration::sea_orm::ColumnTrait;
use migration::sea_orm::EntityTrait;
use migration::sea_orm::QueryFilter;
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;

/// Create or get the existing referral link.
/// This is the link that the user can share to third parties, and get credits.
#[debug_handler]
pub async fn user_referral_link_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
    Query(_params): Query<HashMap<String, String>>,
) -> Web3ProxyResponse {
    // First get the bearer token and check if the user is logged in
    let user = app.bearer_is_authorized(bearer).await?;

    let db_replica = app.db_replica()?;

    // Then get the referral token. If one doesn't exist, create one
    let user_referrer = referrer::Entity::find()
        .filter(referrer::Column::UserId.eq(user.id))
        .one(db_replica.as_ref())
        .await?;

    let (referral_code, status_code) = match user_referrer {
        Some(x) => (x.referral_code, StatusCode::OK),
        None => {
            // Connect to the database for writes
            let db_conn = app.db_conn()?;

            let referral_code = ReferralCode::default().to_string();

            let referrer_entry = referrer::ActiveModel {
                user_id: sea_orm::ActiveValue::Set(user.id),
                referral_code: sea_orm::ActiveValue::Set(referral_code.clone()),
                ..Default::default()
            };
            referrer_entry.save(db_conn).await?;

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

#[debug_handler]
pub async fn user_used_referral_stats(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
    Query(_params): Query<HashMap<String, String>>,
) -> Web3ProxyResponse {
    // First get the bearer token and check if the user is logged in
    let user = app.bearer_is_authorized(bearer).await?;

    let db_replica = app.db_replica()?;

    // Get all referral records associated with this user
    let referrals = referee::Entity::find()
        .filter(referee::Column::UserId.eq(user.id))
        .find_also_related(referrer::Entity)
        .all(db_replica.as_ref())
        .await?;

    // For each related referral person, find the corresponding user-address
    #[derive(Debug, Serialize)]
    struct Info {
        credits_applied_for_referee: Decimal,
        credits_applied_for_referrer: Decimal,
        referral_start_date: DateTime,
        used_referral_code: String,
    }

    let mut out: Vec<Info> = Vec::new();
    for x in referrals.into_iter() {
        let (referral_record, referrer_record) = (x.0, x.1.context("each referral entity should have a referral code associated with it, but this is not the case!")?);

        // // The foreign key is never optional
        // let referring_user = user::Entity::find_by_id(referrer_record.user_id)
        //     .one(db_replica.as_ref())
        //     .await?
        //     .context("Database error, no foreign key found for referring user")?;

        let tmp = Info {
            credits_applied_for_referee: referral_record.one_time_bonus_applied_for_referee,
            credits_applied_for_referrer: referral_record.credits_applied_for_referrer,
            referral_start_date: referral_record.referral_start_date,
            used_referral_code: referrer_record.referral_code,
        };

        // Start inserting json's into this
        out.push(tmp);
    }

    // Turn this into a response
    let response_json = json!({
        "referrals": out,
        "user": user,
    });

    let response = (StatusCode::OK, Json(response_json)).into_response();
    Ok(response)
}

#[debug_handler]
pub async fn user_shared_referral_stats(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
    Query(_params): Query<HashMap<String, String>>,
) -> Web3ProxyResponse {
    // First get the bearer token and check if the user is logged in
    let user = app.bearer_is_authorized(bearer).await?;

    let db_replica = app.db_replica()?;

    // Get all referral records associated with this user
    let query_result = referrer::Entity::find()
        .filter(referrer::Column::UserId.eq(user.id))
        .find_with_related(referee::Entity)
        .all(db_replica.as_ref())
        .await?;

    // collect info about each referral
    #[derive(Debug, Serialize)]
    struct Info {
        credits_applied_for_referee: Decimal,
        credits_applied_for_referrer: Decimal,
        referral_start_date: DateTime,
        referred_address: Address,
    }

    let mut used_referral_code = None;
    let mut referral_info = vec![];

    if let Some((referrer_record, referral_records)) = query_result.into_iter().next() {
        for referral_record in referral_records.into_iter() {
            used_referral_code = Some(referrer_record.referral_code.clone());

            // The foreign key is never optional
            let referred_user = user::Entity::find_by_id(referral_record.user_id)
                .one(db_replica.as_ref())
                .await?
                .context("Database error, no foreign key found for referring user")?;

            let info = Info {
                credits_applied_for_referee: referral_record.one_time_bonus_applied_for_referee,
                credits_applied_for_referrer: referral_record.credits_applied_for_referrer,
                referral_start_date: referral_record.referral_start_date,
                referred_address: Address::from_slice(&referred_user.address),
            };

            // Start inserting json's into this
            referral_info.push(info);
        }
    }

    // Turn this into a response
    let response_json = json!({
        "referrals": referral_info,
        "used_referral_code": used_referral_code,
        "user": user,
    });

    let response = (StatusCode::OK, Json(response_json)).into_response();
    Ok(response)
}
