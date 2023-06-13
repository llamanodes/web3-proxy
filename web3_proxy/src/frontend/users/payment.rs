use crate::app::Web3ProxyApp;
use crate::errors::{Web3ProxyError, Web3ProxyResponse};
use crate::frontend::authorization::login_is_authorized;
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
use entities::{balance, increase_on_chain_balance_receipt, rpc_key, user};
use ethers::abi::AbiEncode;
use ethers::types::{Address, TransactionReceipt, H256};
use hashbrown::HashMap;
use http::StatusCode;
use log::{debug, info, trace};
use migration::sea_orm::prelude::Decimal;
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, TransactionTrait,
};
use migration::{Expr, OnConflict};
use payment_contracts::ierc20::IERC20;
use payment_contracts::payment_factory::{self, PaymentFactory};
use serde_json::json;
use std::num::NonZeroU64;
use std::sync::Arc;

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
    let (_user, _semaphore) = app.bearer_is_authorized(bearer).await?;

    let db_replica = app.db_replica().context("Getting database connection")?;

    // Just return the balance for the user
    let user_balance = balance::Entity::find()
        .filter(balance::Column::UserId.eq(_user.id))
        .one(db_replica.as_ref())
        .await?
        .map(|x| x.total_deposits - x.total_spent_outside_free_tier)
        .unwrap_or_default();

    let response = json!({
        "balance": user_balance,
    });

    // TODO: Gotta create a new table for the spend part
    Ok(Json(response).into_response())
}

/// `GET /user/deposits` -- Use a bearer token to get the user's balance and spend.
///
/// - shows a list of all deposits, including their chain-id, amount and tx-hash
#[debug_handler]
pub async fn user_deposits_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
) -> Web3ProxyResponse {
    let (user, _semaphore) = app.bearer_is_authorized(bearer).await?;

    let db_replica = app.db_replica().context("Getting database connection")?;

    // Filter by user ...
    let receipts = increase_on_chain_balance_receipt::Entity::find()
        .filter(increase_on_chain_balance_receipt::Column::DepositToUserId.eq(user.id))
        .all(db_replica.as_ref())
        .await?;

    // Return the response, all except the user ...
    let mut response = HashMap::new();
    let receipts = receipts
        .into_iter()
        .map(|x| {
            let mut out = HashMap::new();
            out.insert("amount", serde_json::Value::String(x.amount.to_string()));
            out.insert("chain_id", serde_json::Value::Number(x.chain_id.into()));
            out.insert("tx_hash", serde_json::Value::String(x.tx_hash));
            // TODO: log_index
            out
        })
        .collect::<Vec<_>>();
    response.insert(
        "user",
        json!(format!("{:?}", Address::from_slice(&user.address))),
    );
    response.insert("deposits", json!(receipts));

    Ok(Json(response).into_response())
}

/// `POST /user/balance/:tx_hash` -- Manually process a confirmed txid to update a user's balance.
///
/// We will subscribe to events to watch for any user deposits, but sometimes events can be missed.
/// TODO: change this. just have a /tx/:txhash that is open to anyone. rate limit like we rate limit /login
#[debug_handler]
pub async fn user_balance_post(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    InsecureClientIp(ip): InsecureClientIp,
    Path(mut params): Path<HashMap<String, String>>,
) -> Web3ProxyResponse {
    // I suppose this is ok / good, so people don't spam this endpoint as it is not "cheap"
    // we rate limit by ip instead of bearer token so transactions are easy to submit from scripts
    // TODO: if ip is a 10. or a 172., allow unlimited
    login_is_authorized(&app, ip).await?;

    // Get the transaction hash, and the amount that the user wants to top up by.
    // Let's say that for now, 1 credit is equivalent to 1 dollar (assuming any stablecoin has a 1:1 peg)
    let tx_hash: H256 = params
        .remove("tx_hash")
        .ok_or(Web3ProxyError::BadRequest(
            "You have not provided a tx_hash".into(),
        ))?
        .parse()
        .map_err(|err| {
            Web3ProxyError::BadRequest(format!("unable to parse tx_hash: {}", err).into())
        })?;

    let db_conn = app.db_conn().context("query_user_stats needs a db")?;

    // Return early if the tx was already added
    if increase_on_chain_balance_receipt::Entity::find()
        .filter(increase_on_chain_balance_receipt::Column::TxHash.eq(tx_hash.encode_hex()))
        .one(&db_conn)
        .await?
        .is_some()
    {
        // TODO: double check that the transaction is still seen as "confirmed" if it is NOT, we need to remove credits!

        // this will be status code 200, not 204
        let response = Json(json!({
            "result": "success",
            "message": "this transaction was already in the database",
        }))
        .into_response();

        return Ok(response);
    };

    // get the transaction receipt
    let transaction_receipt = app
        .internal_request::<_, Option<TransactionReceipt>>("eth_getTransactionReceipt", (tx_hash,))
        .await?
        .ok_or(Web3ProxyError::BadRequest(
            format!("transaction receipt not found for {}", tx_hash,).into(),
        ))?;

    trace!("Transaction receipt: {:#?}", transaction_receipt);

    // TODO: if the transaction doesn't have enough confirmations yet, add it to a queue to try again later

    let payment_factory_address = app
        .config
        .deposit_factory_contract
        .context("A deposit_contract must be provided in the config to parse payments")?;

    let payment_factory_contract =
        PaymentFactory::new(payment_factory_address, app.internal_provider().clone());

    debug!(
        "Payment Factory Filter: {:?}",
        payment_factory_contract.payment_received_filter()
    );

    // check bloom filter to be sure this transaction contains any relevant logs
    // TODO: This does not work properly right now, get back this eventually
    // TODO: compare to code in llamanodes/web3-this-then-that
    // if let Some(ValueOrArray::Value(Some(x))) = payment_factory_contract
    //     .payment_received_filter()
    //     .filter
    //     .topics[0]
    // {
    //     debug!("Bloom input bytes is: {:?}", x);
    //     debug!("Bloom input bytes is: {:?}", x.as_fixed_bytes());
    //     debug!("Bloom input as hex is: {:?}", hex!(x));
    //     let bloom_input = BloomInput::Raw(hex!(x));
    //     debug!(
    //         "Transaction receipt logs_bloom: {:?}",
    //         transaction_receipt.logs_bloom
    //     );
    //
    //     // do a quick check that this transaction contains the required log
    //     if !transaction_receipt.logs_bloom.contains_input(x) {
    //         return Err(Web3ProxyError::BadRequest("no matching logs found".into()));
    //     }
    // }

    // the transaction might contain multiple relevant logs. collect them all
    let mut response_data = vec![];

    // all or nothing
    let txn = db_conn.begin().await?;

    // parse the logs from the transaction receipt
    for log in transaction_receipt.logs {
        if let Some(true) = log.removed {
            todo!("delete this transaction from the database");
        }

        // Create a new transaction that will be used for joint transaction
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
                .context("no log_index. transaction must not be confirmed")?;

            // the internal provider will handle caching of requests
            let payment_token = IERC20::new(payment_token_address, app.internal_provider().clone());

            // get the decimals for the token
            // hopefully u32 is always enough, because the Decimal crate doesn't accept a larger scale
            // <https://eips.ethereum.org/EIPS/eip-20> uses uint8, but i've seen pretty much every int in practice
            let payment_token_decimals = payment_token.decimals().call().await?.as_u32();
            let mut payment_token_amount = Decimal::from_str_exact(&payment_token_wei.to_string())?;
            // Setting the scale already does the decimal shift, no need to divide a second time
            payment_token_amount.set_scale(payment_token_decimals)?;

            info!(
                "Found deposit transaction for: {:?} {:?} {:?}",
                recipient_account, payment_token_address, payment_token_amount
            );

            let recipient = match user::Entity::find()
                .filter(user::Column::Address.eq(recipient_account.to_fixed_bytes().as_slice()))
                .one(&db_conn)
                .await?
            {
                Some(x) => x,
                None => {
                    let (user, _, _) = register_new_user(&db_conn, recipient_account).await?;

                    user
                }
            };

            // For now we only accept stablecoins
            // And we hardcode the peg (later we would have to depeg this, for example
            // 1$ = Decimal(1) for any stablecoin
            // TODO: Let's assume that people don't buy too much at _once_, we do support >$1M which should be fine for now
            debug!(
                "Arithmetic is: {:?} / 10 ^ {:?} = {:?}",
                payment_token_wei, payment_token_decimals, payment_token_amount
            );

            // create or update the balance
            let balance_entry = balance::ActiveModel {
                id: sea_orm::NotSet,
                total_deposits: sea_orm::Set(payment_token_amount),
                user_id: sea_orm::Set(recipient.id),
                ..Default::default()
            };
            info!("Trying to insert into balance entry: {:?}", balance_entry);
            balance::Entity::insert(balance_entry)
                .on_conflict(
                    OnConflict::new()
                        .values([(
                            balance::Column::TotalDeposits,
                            Expr::col(balance::Column::TotalDeposits).add(payment_token_amount),
                        )])
                        .to_owned(),
                )
                .exec(&txn)
                .await?;

            debug!("Saving tx_hash: {:?}", tx_hash);
            let receipt = increase_on_chain_balance_receipt::ActiveModel {
                tx_hash: sea_orm::ActiveValue::Set(tx_hash.encode_hex()),
                chain_id: sea_orm::ActiveValue::Set(app.config.chain_id),
                // TODO: need a migration that adds log_index
                // TODO: need a migration that adds payment_token_address. will be useful for stats
                amount: sea_orm::ActiveValue::Set(payment_token_amount),
                deposit_to_user_id: sea_orm::ActiveValue::Set(recipient.id),
                ..Default::default()
            };
            info!("Trying to insert receipt {:?}", receipt);

            receipt.save(&txn).await?;

            // Remove all RPC-keys owned by this user from the cache, s.t. rate limits are re-calculated
            let rpc_keys = rpc_key::Entity::find()
                .filter(rpc_key::Column::UserId.eq(recipient.id))
                .all(&txn)
                .await?;

            match NonZeroU64::try_from(recipient.id) {
                Err(_) => {}
                Ok(x) => {
                    app.user_balance_cache.invalidate(&x).await;
                }
            };

            for rpc_key_entity in rpc_keys {
                app.rpc_secret_key_cache
                    .invalidate(&rpc_key_entity.secret_key.into())
                    .await;
            }

            let x = json!({
                "tx_hash": tx_hash,
                "log_index": log_index,
                "token": payment_token_address,
                "amount": payment_token_amount,
            });

            response_data.push(x);
        }
    }

    txn.commit().await?;
    debug!("Saved to db");

    let response = (StatusCode::CREATED, Json(json!(response_data))).into_response();

    Ok(response)
}
