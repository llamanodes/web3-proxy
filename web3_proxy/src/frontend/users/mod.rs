//! Handle registration, logins, and managing account data.
pub mod authentication;
pub mod payment;
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
use entities;
use entities::user;
use migration::sea_orm::{self, ActiveModelTrait};
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
) -> Web3ProxyResponse {
    let (user, _semaphore) = app.bearer_is_authorized(bearer_token).await?;

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

            // TODO: do some basic validation
            // TODO: don't set immediatly, send a confirmation email first
            // TODO: compare first? or is sea orm smart enough to do that for us?
            user.email = sea_orm::Set(Some(x));
        }
    }

    // TODO: what else can we update here? password hash? subscription to newsletter?

    let user = if user.is_changed() {
        let db_conn = app.db_conn().web3_context("Getting database connection")?;

        user.save(&db_conn).await?
    } else {
        // no changes. no need to touch the database
        user
    };

    let user: user::Model = user.try_into().web3_context("Returning updated user")?;

    Ok(Json(user).into_response())
}
