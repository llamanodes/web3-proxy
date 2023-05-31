use crate::app::Web3ProxyApp;
use crate::errors::{Web3ProxyError, Web3ProxyResponse};
use anyhow::{anyhow, Context};
use axum::{
    extract::Path,
    headers::{authorization::Bearer, Authorization},
    response::IntoResponse,
    Extension, Json, TypedHeader,
};
use axum_macros::debug_handler;
use entities::{balance, increase_on_chain_balance_receipt, rpc_key, user, user_tier};
use ethers::abi::{AbiEncode, ParamType};
use ethers::prelude::abigen;
use ethers::types::{Address, TransactionReceipt, H256, U256};
use hashbrown::HashMap;
// use http::StatusCode;
use log::{debug, info, trace, warn};
use migration::sea_orm::ColumnTrait;
use migration::sea_orm::EntityTrait;
use migration::sea_orm::QueryFilter;
use serde_json::json;
use std::num::NonZeroU64;
use std::sync::Arc;

// TODO: do this in a build.rs so that the editor autocomplete and docs are better
abigen!(
    IERC20,
    r#"[
        decimals() -> uint256
        event Transfer(address indexed from, address indexed to, uint256 value)
        event Approval(address indexed owner, address indexed spender, uint256 value)
    ]"#,
);

abigen!(
    PaymentFactory,
    r#"[
        event PaymentReceived(address indexed account, address token, uint256 amount)
        account_to_payment_address(address) -> address
        payment_address_to_account(address) -> address
    ]"#,
);

abigen!(
    PaymentSweeper,
    r#"[
    ]"#,
);

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

    // Return straight false if the tx was already added ...
    if increase_on_chain_balance_receipt::Entity::find()
        .filter(increase_on_chain_balance_receipt::Column::TxHash.eq(tx_hash.encode_hex()))
        .one(&db_conn)
        .await?
        .is_some()
    {
        let response = Json(json!({
            "result": "success",
            "message": "this transaction was already in the database",
        }))
        .into_response();

        return Ok(response);
    }

    // get the transaction receipt
    let transaction_receipt: Option<TransactionReceipt> = app
        .internal_request("eth_getTransactionReceipt", (tx_hash,))
        .await?;

    let transaction_receipt = if let Some(transaction_receipt) = transaction_receipt {
        transaction_receipt
    } else {
        return Err(Web3ProxyError::BadRequest(
            format!("transaction receipt not found for {}", tx_hash,).into(),
        ));
    };

    trace!("Transaction receipt: {:#?}", transaction_receipt);

    // TODO: if the transaction doesn't have enough confirmations yet, add it to a queue to try again later

    let payment_factory_address = app
        .config
        .deposit_factory_contract
        .context("A deposit_contract must be provided in the config to parse payments")?;

    let payment_factory =
        PaymentFactory::new(payment_factory_address, app.internal_provider().clone());

    // there is no need to check accepted tokens. the smart contract already reverts if the token isn't accepted

    // let deposit_log = payment_factory.something?;

    // // TODO: do a quick check that this transaction contains the required log
    // if !transaction_receipt.logs_bloom.contains_input(deposit_log) {
    //     return Err(Web3ProxyError::BadRequest("no matching logs found".into()));
    // }

    // parse the logs from the transaction receipt
    // there might be multiple logs with the event if the transaction is doing things in bulk
    // TODO: change the indexes to be unique on (chain, txhash, log_index)
    for log in transaction_receipt.logs {
        if log.address != payment_factory_address {
            continue;
        }
        // TODO: check the log topic matches our factory
        // TODO: check the log send matches our factory

        let log_index = log
            .log_index
            .context("no log_index. transaction must not be confirmed")?;

        // TODO: get the payment token address out of the event
        let payment_token_address = Address::zero();

        // TODO: get the payment token amount out of the event (wei = the integer unit)
        let payment_token_wei = U256::zero();

        let payment_token = IERC20::new(payment_token_address, app.internal_provider().clone());

        // TODO: get the account the payment was received on behalf of (any account could have sent it)
        let on_behalf_of_address = Address::zero();

        // get the decimals for the token
        let payment_token_decimals = payment_token.decimals().call().await;

        todo!("now what?");
    }

    todo!("now what?");
    /*

    for log in transaction_receipt.logs {
        if log.address != deposit_contract {
            debug!(
                "Out: Log is not relevant, as it is not directed to the deposit contract {:?} {:?}",
                format!("{:?}", log.address),
                deposit_contract
            );
            continue;
        }

        // Get the topics out
        let topic: H256 = log.topics.get(0).unwrap().to_owned();
        if topic != deposit_topic {
            debug!(
                "Out: Topic is not relevant: {:?} {:?}",
                topic, deposit_topic
            );
            continue;
        }

        // TODO: Will this work? Depends how logs are encoded
        let (recipient_account, token, amount): (Address, Address, U256) = match ethers::abi::decode(
            &[
                ParamType::Address,
                ParamType::Address,
                ParamType::Uint(256usize),
            ],
            &log.data,
        ) {
            Ok(tpl) => (
                tpl.get(0)
                    .unwrap()
                    .clone()
                    .into_address()
                    .context("Could not decode recipient")?,
                tpl.get(1)
                    .unwrap()
                    .clone()
                    .into_address()
                    .context("Could not decode token")?,
                tpl.get(2)
                    .unwrap()
                    .clone()
                    .into_uint()
                    .context("Could not decode amount")?,
            ),
            Err(err) => {
                warn!("Out: Could not decode! {:?}", err);
                continue;
            }
        };

        // return early if amount is 0
        if amount == U256::from(0) {
            warn!(
                "Out: Found log has amount = 0 {:?}. This should never be the case according to the smart contract",
                amount
            );
            continue;
        }

        info!(
            "Found deposit transaction for: {:?} {:?} {:?}",
            recipient_account, token, amount
        );

        // Create a new transaction that will be used for joint transaction
        let txn = db_conn.begin().await?;

        // We must (1) lock the user and (2) lock the balance
        // Both balance and lock must be present
        // no need to do OnConflict with this functionality
        // Encoding is inefficient, revisit later
        let recipient = match user::Entity::find()
            .filter(user::Column::Address.eq(recipient_account.encode_hex()))
            .one(db_replica.as_ref())
            .await?
        {
            Some(x) => Ok(x),
            None => Err(Web3ProxyError::BadRequest(
                "The user must have signed up first. They are currently not signed up!".to_string(),
            )),
        }?;

            // For now we only accept stablecoins
            // And we hardcode the peg (later we would have to depeg this, for example
            // 1$ = Decimal(1) for any stablecoin
            // TODO: Let's assume that people don't buy too much at _once_, we do support >$1M which should be fine for now
            debug!("Arithmetic is: {:?} {:?}", amount, decimals);
            debug!(
                "Decimals arithmetic is: {:?} {:?}",
                Decimal::from(amount.as_u128()),
                Decimal::from(10_u64.pow(decimals))
            );
            let mut amount = Decimal::from(amount.as_u128());
            let _ = amount.set_scale(decimals);
            debug!("Amount is: {:?}", amount);

            // Check if the item is in the database. If it is not, then add it into the database
            let user_balance = balance::Entity::find()
                .filter(balance::Column::UserId.eq(recipient.id))
                .one(&db_conn)
                .await?;

            // Get the premium user-tier
            let premium_user_tier = user_tier::Entity::find()
                .filter(user_tier::Column::Title.eq("Premium"))
                .one(&db_conn)
                .await?
                .context("Could not find 'Premium' Tier in user-database")?;

            let txn = db_conn.begin().await?;
            match user_balance {
                Some(user_balance) => {
                    let balance_plus_amount = user_balance.available_balance + amount;
                    info!("New user balance is: {:?}", balance_plus_amount);
                    // Update the entry, adding the balance
                    let mut active_user_balance = user_balance.into_active_model();
                    active_user_balance.available_balance = sea_orm::Set(balance_plus_amount);

                    if balance_plus_amount >= Decimal::new(10, 0) {
                        // Also make the user premium at this point ...
                        let mut active_recipient = recipient.clone().into_active_model();
                        // Make the recipient premium "Effectively Unlimited"
                        active_recipient.user_tier_id = sea_orm::Set(premium_user_tier.id);
                        active_recipient.save(&txn).await?;
                    }

                    debug!("New user balance model is: {:?}", active_user_balance);
                    active_user_balance.save(&txn).await?;
                    // txn.commit().await?;
                    // user_balance
                }
                None => {
                    // Create the entry with the respective balance
                    let active_user_balance = balance::ActiveModel {
                        available_balance: sea_orm::ActiveValue::Set(amount),
                        user_id: sea_orm::ActiveValue::Set(recipient.id),
                        ..Default::default()
                    };

                    if amount >= Decimal::new(10, 0) {
                        // Also make the user premium at this point ...
                        let mut active_recipient = recipient.clone().into_active_model();
                        // Make the recipient premium "Effectively Unlimited"
                        active_recipient.user_tier_id = sea_orm::Set(premium_user_tier.id);
                        active_recipient.save(&txn).await?;
                    }

                    info!("New user balance model is: {:?}", active_user_balance);
                    active_user_balance.save(&txn).await?;
                    // txn.commit().await?;
                    // user_balance // .try_into_model().unwrap()
                }
            };
            debug!("Setting tx_hash: {:?}", tx_hash);
            let receipt = increase_on_chain_balance_receipt::ActiveModel {
                tx_hash: sea_orm::ActiveValue::Set(tx_hash.encode_hex()),
                chain_id: sea_orm::ActiveValue::Set(app.config.chain_id),
                amount: sea_orm::ActiveValue::Set(amount),
                deposit_to_user_id: sea_orm::ActiveValue::Set(recipient.id),
                ..Default::default()
            };

            receipt.save(&txn).await?;
            txn.commit().await?;
            debug!("Saved to db");

            let response = (
                StatusCode::CREATED,
                Json(json!({
                    "tx_hash": tx_hash,
                    "amount": amount
                })),
            )
                .into_response();

            // Return early if the log was added, assume there is at most one valid log per transaction
            return Ok(response);
        }
         */
}
