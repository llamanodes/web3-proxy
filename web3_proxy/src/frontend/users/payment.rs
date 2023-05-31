use crate::app::Web3ProxyApp;
use crate::errors::{Web3ProxyError, Web3ProxyResponse};
use anyhow::Context;
use axum::{
    extract::Path,
    headers::{authorization::Bearer, Authorization},
    response::IntoResponse,
    Extension, Json, TypedHeader,
};
use axum_macros::debug_handler;
use entities::{balance, increase_on_chain_balance_receipt, user};
use ethbloom::Input as BloomInput;
use ethers::abi::{AbiEncode, ParamType};
use ethers::types::{Address, TransactionReceipt, ValueOrArray, H256, U256};
use hashbrown::HashMap;
use http::StatusCode;
use log::{debug, info, trace};
use migration::sea_orm::prelude::Decimal;
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter,
    TransactionTrait,
};
use num_traits::Pow;
use payment_contracts::ierc20::IERC20;
use payment_contracts::payment_factory::{self, PaymentFactory};
use serde_json::json;
use std::str::FromStr;
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
        .map(|x| x.available_balance)
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
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
    Path(mut params): Path<HashMap<String, String>>,
) -> Web3ProxyResponse {
    // I suppose this is ok / good, so people don't spam this endpoint as it is not "cheap"
    // Check that the user is logged-in and authorized
    // The semaphore keeps a user from submitting tons of transactions in parallel which would DOS our backends
    let (_, _semaphore) = app.bearer_is_authorized(bearer).await?;

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
    }

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

    // check bloom filter to be sure this transaction contains any relevant logs
    if let Some(ValueOrArray::Value(Some(x))) = payment_factory_contract
        .payment_received_filter()
        .filter
        .topics[0]
    {
        let bloom_input = BloomInput::Hash(x.as_fixed_bytes());

        // do a quick check that this transaction contains the required log
        if !transaction_receipt.logs_bloom.contains_input(bloom_input) {
            return Err(Web3ProxyError::BadRequest("no matching logs found".into()));
        }
    }

    // the transaction might contain multiple relevant logs. collect them all
    let mut response_data = vec![];

    // all or nothing
    let txn = db_conn.begin().await?;

    // parse the logs from the transaction receipt
    for log in transaction_receipt.logs {
        if let Some(true) = log.removed {
            todo!("delete this transaction from the database");
        }

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

            let decimal_shift = Decimal::from(10).pow(payment_token_decimals as u64);

            let mut payment_token_amount = Decimal::from_str_exact(&payment_token_wei.to_string())?;
            payment_token_amount.set_scale(payment_token_decimals)?;
            payment_token_amount /= decimal_shift;

            info!(
                "Found deposit transaction for: {:?} {:?} {:?}",
                recipient_account, payment_token_address, payment_token_amount
            );

            let recipient = match user::Entity::find()
                .filter(user::Column::Address.eq(recipient_account.encode_hex()))
                .one(&db_conn)
                .await?
            {
                Some(x) => x,
                None => todo!("make their account"),
            };

            // For now we only accept stablecoins
            // And we hardcode the peg (later we would have to depeg this, for example
            // 1$ = Decimal(1) for any stablecoin
            // TODO: Let's assume that people don't buy too much at _once_, we do support >$1M which should be fine for now
            debug!(
                "Arithmetic is: {:?} / 10 ^ {:?} = {:?}",
                payment_token_wei, payment_token_decimals, payment_token_amount
            );

            // Check if the item is in the database. If it is not, then add it into the database
            // TODO: `insert ... on duplicate update` to avoid a race
            let user_balance = balance::Entity::find()
                .filter(balance::Column::UserId.eq(recipient.id))
                .one(&txn)
                .await?;

            match user_balance {
                Some(user_balance) => {
                    // Update the entry, adding the balance
                    let balance_plus_amount = user_balance.available_balance + payment_token_amount;

                    let mut active_user_balance = user_balance.into_active_model();
                    active_user_balance.available_balance = sea_orm::Set(balance_plus_amount);

                    debug!("New user balance: {:?}", active_user_balance);
                    active_user_balance.save(&txn).await?;
                }
                None => {
                    // Create the entry with the respective balance
                    let active_user_balance = balance::ActiveModel {
                        available_balance: sea_orm::ActiveValue::Set(payment_token_amount),
                        user_id: sea_orm::ActiveValue::Set(recipient.id),
                        ..Default::default()
                    };

                    debug!("New user balance: {:?}", active_user_balance);
                    active_user_balance.save(&txn).await?;
                }
            }

            debug!("Setting tx_hash: {:?}", tx_hash);
            let receipt = increase_on_chain_balance_receipt::ActiveModel {
                tx_hash: sea_orm::ActiveValue::Set(tx_hash.encode_hex()),
                chain_id: sea_orm::ActiveValue::Set(app.config.chain_id),
                // TODO: need a migration that adds log_index
                amount: sea_orm::ActiveValue::Set(payment_token_amount),
                deposit_to_user_id: sea_orm::ActiveValue::Set(recipient.id),
                ..Default::default()
            };

            receipt.save(&txn).await?;

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
