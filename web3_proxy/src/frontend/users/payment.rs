use crate::app::Web3ProxyApp;
use crate::balance::Balance;
use crate::errors::{Web3ProxyError, Web3ProxyResponse, Web3ProxyResult};
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
    admin_increase_balance_receipt, increase_on_chain_balance_receipt, rpc_key,
    stripe_increase_balance_receipt, user,
};
use ethers::abi::AbiEncode;
use ethers::types::{Address, Block, TransactionReceipt, TxHash, H256};
use hashbrown::{HashMap, HashSet};
use http::StatusCode;
use migration::sea_orm::prelude::Decimal;
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, EntityTrait, ModelTrait, QueryFilter, TransactionTrait,
};
use payment_contracts::ierc20::IERC20;
use payment_contracts::payment_factory::{self, PaymentFactory};
use serde_json::json;
use std::sync::Arc;
use tracing::{debug, info, trace};

/// Implements any logic related to payments
/// Removed this mainly from "user" as this was getting clogged
///
/// `GET /user/balance` -- Use a bearer token to get the user's balance and spend.
///
/// - show balance in USD
/// - show deposits history (currency, amounts, transaction id)
#[debug_handler]
pub async fn user_balance_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
) -> Web3ProxyResponse {
    let user = app.bearer_is_authorized(bearer).await?;

    let db_replica = app.db_replica()?;

    let user_balance = match Balance::try_from_db(db_replica.as_ref(), user.id).await? {
        None => Balance::default(),
        Some(x) => x,
    };

    Ok(Json(user_balance).into_response())
}

/// `GET /user/deposits/chain` -- Use a bearer token to get the user's balance and spend.
///
/// - shows a list of all deposits, including their chain-id, amount and tx-hash
#[debug_handler]
pub async fn user_chain_deposits_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
) -> Web3ProxyResponse {
    let user = app.bearer_is_authorized(bearer).await?;

    let db_replica = app.db_replica()?;

    // Filter by user ...
    let receipts = increase_on_chain_balance_receipt::Entity::find()
        .filter(increase_on_chain_balance_receipt::Column::DepositToUserId.eq(user.id))
        .all(db_replica.as_ref())
        .await?;

    // Return the response, all except the user ...
    let receipts = receipts
        .into_iter()
        .map(|x| {
            json!({
                "amount": x.amount,
                "chain_id": x.chain_id,
                "tx_hash": x.tx_hash,
            })
        })
        .collect::<Vec<_>>();

    let response = json!({
        "user": Address::from_slice(&user.address),
        "deposits": receipts,
    });

    Ok(Json(response).into_response())
}

/// `GET /user/deposits/stripe` -- Use a bearer token to get the user's balance and spend.
///
/// - shows a list of all deposits done through stripe
#[debug_handler]
pub async fn user_stripe_deposits_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
) -> Web3ProxyResponse {
    let user = app.bearer_is_authorized(bearer).await?;

    let db_replica = app.db_replica()?;

    // Filter by user ...
    let receipts = stripe_increase_balance_receipt::Entity::find()
        .filter(stripe_increase_balance_receipt::Column::DepositToUserId.eq(Some(user.id)))
        .all(db_replica.as_ref())
        .await?;

    // Return the response, all except the user ...
    let receipts = receipts
        .into_iter()
        .map(|x| {
            json!({
                "id": x.id,
                "stripe_payment_intend_id": x.stripe_payment_intend_id,
                "deposit_to_user_id": x.deposit_to_user_id,
                "amount": x.amount,
                "currency": x.currency,
                "status": x.status,
                "description": x.description,
                "date_created": x.date_created
            })
        })
        .collect::<Vec<_>>();

    let response = json!({
        "user": Address::from_slice(&user.address),
        "deposits": receipts,
    });

    Ok(Json(response).into_response())
}

/// `GET /user/deposits/admin` -- Use a bearer token to get the user's balance and spend.
///
/// - shows a list of all deposits done by admins
#[debug_handler]
pub async fn user_admin_deposits_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
) -> Web3ProxyResponse {
    let user = app.bearer_is_authorized(bearer).await?;

    let db_replica = app.db_replica()?;

    // Filter by user ...
    let receipts = admin_increase_balance_receipt::Entity::find()
        .filter(admin_increase_balance_receipt::Column::DepositToUserId.eq(user.id))
        .all(db_replica.as_ref())
        .await?;

    // Return the response, all except the user ...
    let receipts = receipts
        .into_iter()
        .map(|x| {
            json!({
                "id": x.id,
                "amount": x.amount,
                "deposit_to_user_id": x.deposit_to_user_id,
                "note": x.note,
            })
        })
        .collect::<Vec<_>>();

    let response = json!({
        "user": Address::from_slice(&user.address),
        "deposits": receipts,
    });

    Ok(Json(response).into_response())
}

/// `POST /user/balance/:tx_hash` -- Process a confirmed txid to update a user's balance.
#[debug_handler]
pub async fn user_balance_post(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    ip: Option<InsecureClientIp>,
    Path(mut params): Path<HashMap<String, String>>,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
) -> Web3ProxyResponse {
    // rate limit by bearer token **OR** IP address
    let authorization = if let Some(TypedHeader(Authorization(bearer))) = bearer {
        app.bearer_is_authorized(bearer).await?;

        // TODO: is handling this as internal fine?
        Web3ProxyAuthorization::internal(app.db_conn().ok().cloned())?
    } else if let Some(InsecureClientIp(ip)) = ip {
        login_is_authorized(&app, ip).await?
    } else {
        return Err(Web3ProxyError::AccessDenied("no bearer token or ip".into()));
    };

    // Get the transaction hash
    let tx_hash: H256 = params
        .remove("tx_hash")
        .ok_or(Web3ProxyError::BadRequest(
            "You have not provided a tx_hash".into(),
        ))?
        .parse()
        .map_err(|err| {
            Web3ProxyError::BadRequest(format!("unable to parse tx_hash: {}", err).into())
        })?;

    let db_conn = app.db_conn()?;

    let authorization = Arc::new(authorization);

    // get the transaction receipt
    let transaction_receipt = app
        .authorized_request::<_, Option<TransactionReceipt>>(
            "eth_getTransactionReceipt",
            (tx_hash,),
            authorization.clone(),
        )
        .await?;

    // check for uncles
    let mut find_uncles = increase_on_chain_balance_receipt::Entity::find()
        .filter(increase_on_chain_balance_receipt::Column::TxHash.eq(tx_hash.encode_hex()))
        .filter(increase_on_chain_balance_receipt::Column::ChainId.eq(app.config.chain_id));

    let tx_pending =
        if let Some(block_hash) = transaction_receipt.as_ref().and_then(|x| x.block_hash) {
            // check for uncles
            // this transaction is confirmed
            // any rows in the db with a block hash that doesn't match the receipt should be deleted
            find_uncles = find_uncles.filter(
                increase_on_chain_balance_receipt::Column::BlockHash.ne(block_hash.encode_hex()),
            );

            false
        } else {
            // no block_hash to check
            // this transaction is not confirmed
            // any rows in the db should be deleted
            true
        };

    let uncle_hashes = find_uncles.all(db_conn).await?;

    let uncle_hashes: HashSet<_> = uncle_hashes
        .into_iter()
        .map(|x| serde_json::from_str(x.block_hash.as_str()).unwrap())
        .collect();

    for uncle_hash in uncle_hashes.into_iter() {
        if let Some(x) = handle_uncle_block(&app, &authorization, uncle_hash).await? {
            info!("balance changes from uncle: {:#?}", x);
        }
    }

    if tx_pending {
        // the transaction isn't confirmed. return early
        // TODO: BadRequest, or something else?
        return Err(Web3ProxyError::BadRequest(
            "this transaction has not confirmed yet. Please try again later.".into(),
        ));
    }

    let transaction_receipt =
        transaction_receipt.expect("if tx_pending is false, transaction_receipt must be set");

    let block_hash = transaction_receipt
        .block_hash
        .expect("if tx_pending is false, block_hash must be set");

    trace!("Transaction receipt: {:#?}", transaction_receipt);

    // TODO: if the transaction doesn't have enough confirmations yet, add it to a queue to try again later
    // 1 confirmation should be fine though

    let txn = db_conn.begin().await?;

    // if the transaction is already saved, return early
    if increase_on_chain_balance_receipt::Entity::find()
        .filter(increase_on_chain_balance_receipt::Column::TxHash.eq(tx_hash.encode_hex()))
        .filter(increase_on_chain_balance_receipt::Column::ChainId.eq(app.config.chain_id))
        .filter(increase_on_chain_balance_receipt::Column::BlockHash.eq(block_hash.encode_hex()))
        .one(&txn)
        .await?
        .is_some()
    {
        return Ok(Json(json!({
            "result": "tx_hash already saved",
        }))
        .into_response());
    };

    let payment_factory_address = app
        .config
        .deposit_factory_contract
        .context("A deposit_contract must be provided in the config to parse payments")?;

    let payment_factory_contract =
        PaymentFactory::new(payment_factory_address, app.internal_provider().clone());

    // TODO: check bloom filters

    // the transaction might contain multiple relevant logs. collect them all
    let mut response_data = vec![];
    for log in transaction_receipt.logs {
        if let Some(true) = log.removed {
            // TODO: do we need to make sure this row is deleted? it should be handled by `handle_uncle_block`
            continue;
        }

        // Parse the log into an event
        if let Ok(event) = payment_factory_contract
            .decode_event::<payment_factory::PaymentReceivedFilter>(
                "PaymentReceived",
                log.topics,
                log.data,
            )
        {
            let recipient_account = event.account;
            let payment_token_address = event.token;
            let payment_token_wei = event.amount;

            // there is no need to check that payment_token_address is an allowed token
            // the smart contract already reverts if the token isn't accepted

            // we used to skip here if amount is 0, but that means the txid wouldn't ever show up in the database which could be confusing
            // its irrelevant though because the contract already reverts for 0 value

            let log_index = log
                .log_index
                .context("no log_index. transaction must not be confirmed")?
                .as_u64();

            // the internal provider will handle caching of requests
            let payment_token = IERC20::new(payment_token_address, app.internal_provider().clone());

            // get the decimals for the token
            // hopefully u32 is always enough, because the Decimal crate doesn't accept a larger scale
            // <https://eips.ethereum.org/EIPS/eip-20> uses uint8, but i've seen pretty much every int in practice
            let payment_token_decimals = payment_token.decimals().call().await?.as_u32();
            let mut payment_token_amount = Decimal::from_str_exact(&payment_token_wei.to_string())?;
            // Setting the scale already does the decimal shift, no need to divide a second time
            payment_token_amount.set_scale(payment_token_decimals)?;

            trace!(
                "found deposit event for: {:?} {:?} {:?}",
                recipient_account,
                payment_token_address,
                payment_token_amount
            );

            let recipient = match user::Entity::find()
                .filter(user::Column::Address.eq(recipient_account.as_bytes()))
                .one(&txn)
                .await?
            {
                Some(x) => x,
                None => {
                    let (user, _) = register_new_user(&txn, recipient_account).await?;

                    user
                }
            };

            // For now we only accept stablecoins. This will need conversions if we accept other tokens.
            // 1$ = Decimal(1) for any stablecoin
            // TODO: Let's assume that people don't buy too much at _once_, we do support >$1M which should be fine for now
            // TODO: double check. why >$1M? Decimal type in the database?
            trace!(
                "Arithmetic is: {:?} / 10 ^ {:?} = {:?}",
                payment_token_wei,
                payment_token_decimals,
                payment_token_amount
            );

            trace!("Saving log {} of txid {:?}", log_index, tx_hash);
            let receipt = increase_on_chain_balance_receipt::ActiveModel {
                id: sea_orm::ActiveValue::NotSet,
                amount: sea_orm::ActiveValue::Set(payment_token_amount),
                block_hash: sea_orm::ActiveValue::Set(block_hash.encode_hex()),
                chain_id: sea_orm::ActiveValue::Set(app.config.chain_id),
                deposit_to_user_id: sea_orm::ActiveValue::Set(recipient.id),
                log_index: sea_orm::ActiveValue::Set(log_index),
                token_address: sea_orm::ActiveValue::Set(payment_token_address.encode_hex()),
                tx_hash: sea_orm::ActiveValue::Set(tx_hash.encode_hex()),
            };
            trace!("Trying to insert receipt {:?}", receipt);

            receipt.save(&txn).await?;

            // Remove all RPC-keys owned by this user from the cache, s.t. rate limits are re-calculated
            let rpc_keys = rpc_key::Entity::find()
                .filter(rpc_key::Column::UserId.eq(recipient.id))
                .all(&txn)
                .await?;

            app.user_balance_cache.invalidate(&recipient.id).await;

            for rpc_key_entity in rpc_keys {
                app.rpc_secret_key_cache
                    .invalidate(&rpc_key_entity.secret_key.into())
                    .await;
            }

            let x = json!({
                "amount": payment_token_amount,
                "block_hash": block_hash,
                "log_index": log_index,
                "recipient_account": recipient_account,
                "token": payment_token_address,
                "tx_hash": tx_hash,
            });

            debug!("deposit data: {:#?}", x);

            response_data.push(x);
        }
    }

    txn.commit().await?;

    let response = (StatusCode::CREATED, Json(json!(response_data))).into_response();

    Ok(response)
}

/// `POST /user/balance_uncle/:uncle_hash` -- Process an uncle block to potentially update a user's balance.
#[debug_handler]
pub async fn user_balance_uncle_post(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    InsecureClientIp(ip): InsecureClientIp,
    Path(mut params): Path<HashMap<String, String>>,
) -> Web3ProxyResponse {
    let authorization = login_is_authorized(&app, ip).await?;

    // Get the transaction hash, and the amount that the user wants to top up by.
    // Let's say that for now, 1 credit is equivalent to 1 dollar (assuming any stablecoin has a 1:1 peg)
    let uncle_hash: H256 = params
        .remove("uncle_hash")
        .ok_or(Web3ProxyError::BadRequest(
            "You have not provided a uncle_hash".into(),
        ))?
        .parse()
        .map_err(|err| {
            Web3ProxyError::BadRequest(format!("unable to parse uncle_hash: {}", err).into())
        })?;

    let authorization = Arc::new(authorization);

    if let Some(x) = handle_uncle_block(&app, &authorization, uncle_hash).await? {
        Ok(Json(x).into_response())
    } else {
        // TODO: is BadRequest the right error to use?
        Err(Web3ProxyError::BadRequest("block is not an uncle".into()))
    }
}

pub async fn handle_uncle_block(
    app: &Arc<Web3ProxyApp>,
    authorization: &Arc<Web3ProxyAuthorization>,
    uncle_hash: H256,
) -> Web3ProxyResult<Option<HashMap<u64, Decimal>>> {
    info!("handling uncle: {:?}", uncle_hash);

    // cancel if uncle_hash is actually a confirmed block
    if app
        .authorized_request::<_, Option<Block<TxHash>>>(
            "eth_getBlockByHash",
            (uncle_hash, false),
            authorization.clone(),
        )
        .await
        .context("eth_getBlockByHash failed")?
        .is_some()
    {
        return Ok(None);
    }

    // user_id -> balance that we need to subtract
    let mut reversed_balances: HashMap<u64, Decimal> = HashMap::new();

    let txn = app.db_transaction().await?;

    // delete any deposit txids with uncle_hash
    for reversed_deposit in increase_on_chain_balance_receipt::Entity::find()
        .filter(increase_on_chain_balance_receipt::Column::BlockHash.eq(uncle_hash.encode_hex()))
        .all(&txn)
        .await?
    {
        let reversed_balance = reversed_balances
            .entry(reversed_deposit.deposit_to_user_id)
            .or_default();

        *reversed_balance += reversed_deposit.amount;

        // TODO: instead of delete, mark as uncled? seems like it would bloat the db unnecessarily. a stat should be enough
        reversed_deposit.delete(&txn).await?;
    }

    debug!("removing balances: {:#?}", reversed_balances);

    txn.commit().await?;

    Ok(Some(reversed_balances))
}
