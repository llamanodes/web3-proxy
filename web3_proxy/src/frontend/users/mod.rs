//! Handle registration, logins, and managing account data.
pub mod authentication;
pub mod payment;
pub mod payment_stripe;
pub mod referral;
pub mod rpc_keys;
pub mod stats;
pub mod subuser;

use crate::app::Web3ProxyApp;
use crate::errors::{Web3ProxyError, Web3ProxyErrorContext, Web3ProxyResponse};
use axum::{
    headers::{authorization::Bearer, Authorization},
    response::IntoResponse,
    Extension, Json, TypedHeader,
};
use axum_macros::debug_handler;
use check_if_email_exists::{check_email, CheckEmailInput, Reachable};
use entities::{self, referee, referrer, user};
use migration::sea_orm::{self, ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter};
use serde::Deserialize;
use std::sync::Arc;

/// `GET /user` -- Use a bearer token to get the user's profile.
///
/// - the email address of a user if they opted in to get contacted via email
///
/// TODO: this will change as we add better support for secondary users.
#[debug_handler]
pub async fn user_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer_token)): TypedHeader<Authorization<Bearer>>,
) -> Web3ProxyResponse {
    let user = app.bearer_is_authorized(bearer_token).await?;

    Ok(Json(user).into_response())
}

/// the JSON input to the `post_user` handler.
/// TODO: what else can we update here? password hash? subscription to newsletter?
#[derive(Debug, Deserialize)]
pub struct UserPost {
    email: Option<String>,
    referral_code: Option<String>,
}

/// `POST /user` -- modify the account connected to the bearer token in the `Authentication` header.
#[debug_handler]
pub async fn user_post(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer_token)): TypedHeader<Authorization<Bearer>>,
    Json(payload): Json<UserPost>,
) -> Web3ProxyResponse {
    let user = app.bearer_is_authorized(bearer_token).await?;

    let user_id = user.id;

    let mut user: user::ActiveModel = user.into();

    // update the email address
    if let Some(x) = payload.email {
        // TODO: only Set if no change
        if x.is_empty() {
            user.email = sea_orm::Set(None);
        } else {
            // Make a quick check if the e-mail provide is active
            let check_email_input = CheckEmailInput::new(x.clone());
            // Verify this input, using async/await syntax.
            let result = check_email(&check_email_input).await;

            // Let's be very chill about the validity of e-mails, and only error if the Syntax / SMPT / MX does not work
            if let Reachable::Invalid = result.is_reachable {
                return Err(Web3ProxyError::BadRequest(
                    "The e-mail address you provided seems invalid".into(),
                ));
            }

            // TODO: send a confirmation email first before marking this email address as validated
            user.email = sea_orm::Set(Some(x));
        }
    }

    let txn = app.db_transaction().await?;

    // update the referral code IFF they do not already have one set
    if let Some(x) = payload.referral_code {
        let desired_referral_code = referrer::Entity::find()
            .filter(referrer::Column::ReferralCode.like(&x))
            .one(&txn)
            .await?
            .web3_context(format!("posted referral code does not exist"))?;

        // make sure the user doesn't already have a referral code set
        if let Some(existing_referee) = referee::Entity::find()
            .filter(referee::Column::UserId.eq(user_id))
            .one(&txn)
            .await?
        {
            if existing_referee.used_referral_code == desired_referral_code.id {
                // code was already set this code. nothing to do
            } else {
                // referral code cannot change! error!
                return Err(Web3ProxyError::BadRequest(
                    "referral code cannot be changed".into(),
                ));
            }
        } else {
            // no existing referral code. set one now
            let new_referee = referee::ActiveModel {
                used_referral_code: sea_orm::Set(desired_referral_code.id),
                user_id: sea_orm::Set(user_id),
                ..Default::default()
            };

            new_referee.save(&txn).await?;
        }
    }

    let user = if user.is_changed() {
        user.save(&txn).await?
    } else {
        // no changes. no need to touch the database
        user
    };

    txn.commit().await?;

    let user: user::Model = user.try_into().web3_context("Returning updated user")?;

    Ok(Json(user).into_response())
}
