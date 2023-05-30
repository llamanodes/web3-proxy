use crate::app::Web3ProxyApp;
use crate::frontend::errors::{Web3ProxyError, Web3ProxyResponse};
use anyhow::{anyhow, Context};
use axum::{
    extract::Path,
    headers::{authorization::Bearer, Authorization},
    response::IntoResponse,
    Extension, Json, TypedHeader,
};
use axum_macros::debug_handler;
use entities::{balance, increase_on_chain_balance_receipt, user, user_tier};
use ethers::abi::{AbiEncode, ParamType};
use ethers::prelude::abigen;
use ethers::types::{Address, TransactionReceipt, H256, U256};
use hashbrown::HashMap;
// use http::StatusCode;
use log::{debug, info, trace, warn};
// use migration::sea_orm;
// use migration::sea_orm::prelude::Decimal;
// use migration::sea_orm::ActiveModelTrait;
use migration::sea_orm::ColumnTrait;
use migration::sea_orm::EntityTrait;
// use migration::sea_orm::IntoActiveModel;
use migration::sea_orm::QueryFilter;
// use migration::sea_orm::TransactionTrait;
use serde_json::json;
use std::sync::Arc;

abigen!(
    IERC20,
    r#"[
        event Transfer(address indexed from, address indexed to, uint256 value)
        event Approval(address indexed owner, address indexed spender, uint256 value)
    ]"#,
);

abigen!(
    PaymentFactory,
    r#"[
        event PaymentReceived(address indexed account, address token, uint256 amount)
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
        // TODO: map_err so this becomes a 500. routing must be bad
        .ok_or(Web3ProxyError::BadRequest(
            "You have not provided the tx_hash in which you paid in".to_string(),
        ))?
        .parse()
        .context("unable to parse tx_hash")?;

    let db_conn = app.db_conn().context("query_user_stats needs a db")?;
    // let db_replica = app
    //     .db_replica()
    //     .context("query_user_stats needs a db replica")?;

    // Return straight false if the tx was already added ...
    // TODO: TxHash being string
    let receipt = increase_on_chain_balance_receipt::Entity::find()
        .filter(increase_on_chain_balance_receipt::Column::TxHash.eq(tx_hash.encode_hex()))
        .one(&db_conn)
        .await?;
    if receipt.is_some() {
        return Err(Web3ProxyError::BadRequest(
            "The transaction you provided has already been accounted for!".to_string(),
        ));
    }
    debug!("Receipt: {:?}", receipt);

    // Iterate through all logs, and add them to the transaction list if there is any
    // Address will be hardcoded in the config
    let transaction_receipt: TransactionReceipt = app
        .internal_request("eth_getTransactionReceipt", (tx_hash,))
        .await?;

    trace!("Transaction receipt: {:#?}", transaction_receipt);

    // there is no need to check accepted tokens. the smart contract already does that

    // parse the log from the transaction receipt to get the token address,
    /*
    event PaymentReceived:
        account: indexed(address)
        token: address
        amount: uint256
     */

    // TODO: get the decimals for the token

    // Go through all logs, this should prob capture it,
    // At least according to this SE logs are just concatenations of the underlying types (like a struct..)
    // https://ethereum.stackexchange.com/questions/87653/how-to-decode-log-event-of-my-transaction-log

    todo!("refactor this to use new helpers");

    /*
    let deposit_contract = match app.config.deposit_factory_contract {
        Some(x) => Ok(x),
        None => Err(Web3ProxyError::Anyhow(anyhow!(
            "A deposit_contract must be provided in the config to parse payments"
        ))),
    }?;
    let deposit_topic = match app.config.deposit_topic {
        Some(x) => Ok(x),
        None => Err(Web3ProxyError::Anyhow(anyhow!(
            "A deposit_topic must be provided in the config to parse payments"
        ))),
    }?;

    // Make sure there is only a single log within that transaction ...
    // I don't know how to best cover the case that there might be multiple logs inside

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

    Err(Web3ProxyError::BadRequest(
        "No such transaction was found, or token is not supported!".to_string(),
    ))
}
