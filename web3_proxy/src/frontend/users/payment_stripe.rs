use crate::app::Web3ProxyApp;
use crate::errors::{Web3ProxyError, Web3ProxyErrorContext, Web3ProxyResponse, Web3ProxyResult};
use crate::frontend::authorization::{
    login_is_authorized, Authorization as Web3ProxyAuthorization,
};
use crate::frontend::users::authentication::register_new_user;
use anyhow::Context;
use axum::{
    extract::Path,
    headers::{authorization::Bearer, Authorization},
    response::IntoResponse,
    Extension, Json, TypedHeader,
};
use axum_client_ip::InsecureClientIp;
use axum_macros::debug_handler;
use entities::{
    balance, increase_on_chain_balance_receipt, rpc_key, stripe_increase_balance_receipt, user,
};
use ethers::abi::AbiEncode;
use ethers::types::{Address, Block, TransactionReceipt, TxHash, H256};
use hashbrown::{HashMap, HashSet};
use http::{HeaderMap, StatusCode};
use migration::sea_orm::prelude::Decimal;
use migration::sea_orm::{
    self, ActiveModelTrait, ActiveValue, ColumnTrait, EntityTrait, IntoActiveModel, ModelTrait,
    QueryFilter, QuerySelect, TransactionTrait,
};
use migration::{Expr, OnConflict};
use payment_contracts::ierc20::IERC20;
use payment_contracts::payment_factory::{self, PaymentFactory};
use serde_json::json;
use std::num::NonZeroU64;
use std::sync::Arc;
use stripe::Webhook;
use tracing::{debug, error, info, trace};

/// `GET /user/balance/stripe` -- Use a bearer token to get the user's balance and spend.
///
/// - shows a list of all stripe deposits, all fields from entity
#[debug_handler]
pub async fn user_stripe_deposits_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
) -> Web3ProxyResponse {
    let (user, _semaphore) = app.bearer_is_authorized(bearer).await?;

    let db_replica = app.db_replica().context("Getting database connection")?;

    // Filter by user ...
    let receipts = stripe_increase_balance_receipt::Entity::find()
        .filter(increase_on_chain_balance_receipt::Column::DepositToUserId.eq(user.id))
        .all(db_replica.as_ref())
        .await?;

    // Return the response, all except the user ...
    let response = json!({
        "user": Address::from_slice(&user.address),
        "deposits": receipts,
    });

    Ok(Json(response).into_response())
}

/// `POST /user/balance/stripe/` -- Process a stripe transaction;
/// this endpoint is called from the webhook with the user_id parameter in the request
#[debug_handler]
pub async fn user_balance_stripe_post(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ip: InsecureClientIp,
    headers: HeaderMap,
    Path(mut params): Path<HashMap<String, String>>,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
) -> Web3ProxyResponse {
    // rate limit by bearer token **OR** IP address
    let (authorization, _semaphore) = if let Some(TypedHeader(Authorization(bearer))) = bearer {
        let (_, semaphore) = app.bearer_is_authorized(bearer).await?;

        // TODO: is handling this as internal fine?
        let authorization = Web3ProxyAuthorization::internal(app.db_conn())?;

        (authorization, Some(semaphore))
    } else {
        let InsecureClientIp(ip) = ip;
        let authorization = login_is_authorized(&app, ip).await?;
        (authorization, None)
    };

    let recipient_user_id: u64 = params
        .remove("user_id")
        .ok_or(Web3ProxyError::BadRouting)?
        .parse()
        .or(Err(Web3ProxyError::ParseAddressError))?;

    // Get the payload, and the header
    let payload = params.remove("data").ok_or(Web3ProxyError::BadRequest(
        "You have not provided a 'data' for the Stripe payload".into(),
    ))?;

    // TODO Get this from the header
    let signature = headers
        .get("STRIPE_SIGNATURE")
        .ok_or(Web3ProxyError::BadRequest(
            "You have not provided a 'STRIPE_SIGNATURE' for the Stripe payload".into(),
        ))?
        .to_str()
        .web3_context("Could not parse stripe signature as byte-string")?;

    // Now parse the payload and signature
    // TODO: Move env variable elsewhere
    let event = Webhook::construct_event(
        &payload,
        &signature,
        app.config
            .stripe_api_key
            .clone()
            .web3_context("Stripe API key not found in config!")?
            .as_str(),
    )
    .context(Web3ProxyError::BadRequest(
        "Could not parse the stripe webhook request!".into(),
    ))?;

    let intent = match event.data.object {
        stripe::EventObject::PaymentIntent(intent) => intent,
        _ => return Ok("Received irrelevant webhook".into_response()),
    };
    debug!("Found PaymentIntent Event: {:?}", intent);

    if intent.status.as_str() != "succeeded" {
        return Ok("Received Webhook".into_response());
    }

    let db_conn = app.db_conn().context("query_user_stats needs a db")?;

    if stripe_increase_balance_receipt::Entity::find()
        .filter(
            stripe_increase_balance_receipt::Column::StripePaymentIntendId.eq(intent.id.as_str()),
        )
        .one(&db_conn)
        .await?
        .is_some()
    {
        return Ok("Payment was already recorded".into_response());
    };

    let recipient: Option<user::Model> = user::Entity::find()
        .filter(user::Column::Id.eq(recipient_user_id))
        .one(&db_conn)
        .await?;

    // we do a fixed 2 decimal points because we only accept USD for now
    let amount = Decimal::new(intent.amount, 2);
    let recipient_id: Option<u64> = recipient.as_ref().map_or(None, |x| Some(x.id));
    let insert_receipt_model = stripe_increase_balance_receipt::ActiveModel {
        id: Default::default(),
        deposit_to_user_id: sea_orm::Set(recipient_id),
        amount: sea_orm::Set(amount),
        stripe_payment_intend_id: sea_orm::Set(intent.id.as_str().to_string()),
        currency: sea_orm::Set(intent.currency.to_string()),
        status: sea_orm::Set(intent.status.to_string()),
        description: sea_orm::Set(intent.description),
        date_created: Default::default(),
    };

    // In all these cases, we should record the transaction, but not increase the balance
    let txn = db_conn.begin().await?;

    // Assert that it's usd
    if intent.currency.to_string() != "USD" || !recipient.is_some() {
        // In this case I should probably still save it to the database,
        // but not increase balance (this should be refunded)
        // TODO: I suppose we could send a refund request right away from here
        error!(
            "Please refund this transaction! Currency: {} - Recipient: {} - StripePaymentIntendId {}",
            intent.currency, &recipient_user_id, intent.id
        );
        let _ = insert_receipt_model.save(&txn);
        txn.commit().await?;
        return Ok("Received Webhook".into_response());
    }
    // Otherwise, also increase the balance ...
    match recipient {
        Some(recipient) => {
            // Create a balance update as well
            let balance_entry = balance::ActiveModel {
                id: sea_orm::NotSet,
                total_deposits: sea_orm::Set(amount),
                user_id: sea_orm::Set(recipient.id),
                ..Default::default()
            };
            trace!("Trying to insert into balance entry: {:?}", balance_entry);
            balance::Entity::insert(balance_entry)
                .on_conflict(
                    OnConflict::new()
                        .values([(
                            balance::Column::TotalDeposits,
                            Expr::col(balance::Column::TotalDeposits).add(amount),
                        )])
                        .to_owned(),
                )
                .exec(&txn)
                .await
                .web3_context("increasing balance")?;

            let _ = insert_receipt_model.save(&txn);

            let recipient_rpc_keys = rpc_key::Entity::find()
                .filter(rpc_key::Column::UserId.eq(recipient.id))
                .all(&txn)
                .await?;

            txn.commit().await?;

            // Finally invalidate the cache as well
            match NonZeroU64::try_from(recipient.id) {
                Err(_) => {}
                Ok(x) => {
                    app.user_balance_cache.invalidate(&x).await;
                }
            };
            for rpc_key_entity in recipient_rpc_keys {
                app.rpc_secret_key_cache
                    .invalidate(&rpc_key_entity.secret_key.into())
                    .await;
            }
        }
        None => {
            return Err(Web3ProxyError::BadResponse(
                "We just checked if the recipient is not none, it should've branched before!"
                    .into(),
            ))
        }
    };

    Ok("Received webhook".into_response())
}
