use crate::app::Web3ProxyApp;
use crate::frontend::authorization::{Authorization as InternalAuthorization, RequestMetadata};
use crate::frontend::errors::{Web3ProxyError, Web3ProxyResponse};
use crate::jsonrpc::{
    JsonRpcForwardedResponse, JsonRpcForwardedResponseEnum, JsonRpcRequest, JsonRpcRequestEnum,
};
use crate::rpcs::request::OpenRequestResult;
use anyhow::Context;
use axum::{
    extract::{Path, Query},
    headers::{authorization::Bearer, Authorization},
    response::IntoResponse,
    Extension, Json, TypedHeader,
};
use axum_macros::debug_handler;
use entities::{balance, increase_balance_receipt, user};
use ethers::abi::{AbiEncode, ParamType};
use ethers::types::{Address, TransactionReceipt, H256, U256};
use ethers::utils::{hex, keccak256};
use hashbrown::HashMap;
use http::{HeaderValue, StatusCode};
use log::{debug, warn, Level};
use migration::sea_orm;
use migration::sea_orm::prelude::Decimal;
use migration::sea_orm::ActiveModelTrait;
use migration::sea_orm::ColumnTrait;
use migration::sea_orm::EntityTrait;
use migration::sea_orm::IntoActiveModel;
use migration::sea_orm::QueryFilter;
use migration::sea_orm::TransactionTrait;
use serde_json::json;
use serde_json::value::RawValue;
use std::default::default;
use std::sync::Arc;

/// Implements any logic related to payments
/// Removed this mainly from "user" as this was getting clogged

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
    // Check that the user is logged-in and authorized. We don't need a semaphore here btw
    let (caller, _semaphore) = app.bearer_is_authorized(bearer).await?;

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

    // We don't check the trace, the transaction must be a naive, simple send transaction (for now at least...)
    // TODO: Get the respective transaction ...
    let db_conn = app.db_conn().context("query_user_stats needs a db")?;
    let db_replica = app
        .db_replica()
        .context("query_user_stats needs a db replica")?;

    // Return straight false if the tx was already added ...
    let receipt = increase_balance_receipt::Entity::find()
        .filter(increase_balance_receipt::Column::TxHash.eq(hex::encode(tx_hash)))
        .one(&db_conn)
        .await?;
    if receipt.is_some() {
        return Err(Web3ProxyError::BadRequest(
            "The transaction you provided has already been accounted for!".to_string(),
        ));
    }
    debug!("Receipt: {:?}", receipt);
    // Just iterate through all logs, and add them to the transaction list if there is any
    // Address will be hardcoded in the config
    let authorization = Arc::new(InternalAuthorization::internal(None).unwrap());

    // Just make an rpc request, idk if i need to call this super extensive code
    // I suppose this is ok / good, so people don't spam this endpoint

    // First, get the transaction receipt
    let transaction_receipt: &str = match app
        .proxy_web3_rpc(
            authorization.clone(),
            JsonRpcRequestEnum::Single(JsonRpcRequest {
                method: "eth_getTransactionReceipt".to_owned(),
                params: Some(serde_json::Value::Array(vec![serde_json::Value::String(
                    format!("0x{}", hex::encode(tx_hash)),
                )])),
                ..Default::default()
            }),
        )
        .await?
        .0
    {
        JsonRpcForwardedResponseEnum::Single(response) => match response.result {
            Some(raw_result) => Ok(raw_result.get()),
            None => Err(Web3ProxyError("Transaction Hash was not found!".to_owned())),
        },
        _ => Err(Web3ProxyError::BadRequest(
            "Transaction Hash was not found!".to_owned(),
        )),
    }?;

    let mut accepted_tokens_request_object: HashMap<String, serde_json::Value> = HashMap::new();
    // We want to send a request to the contract
    accepted_tokens_request_object.insert(
        "to".to_owned(),
        serde_json::Value::String(app.config.deposit_contract.clone()),
    );
    // We then want to include the function that we want to call
    accepted_tokens_request_object.insert(
        "data".to_owned(),
        serde_json::Value::String(hex::encode(keccak256("get_approved_tokens()".as_bytes()))),
    );
    let accepted_token: &str = match app
        .proxy_web3_rpc(
            authorization.clone(),
            JsonRpcRequestEnum::Single(JsonRpcRequest {
                method: "eth_call".to_owned(),
                params: Some(serde_json::Value::Object(
                    accepted_tokens_request_object.into(),
                )),
                ..Default::default()
            }),
        )
        .await?
        .0
    {
        JsonRpcForwardedResponseEnum::Single(response) => match response.result {
            Some(raw_result) => Ok(raw_result.get()),
            None => Err(Web3ProxyError("Transaction Hash was not found!".to_owned())),
        },
        _ => Err(Web3ProxyError::BadRequest(
            "Transaction Hash was not found!".to_owned(),
        )),
    }?;

    // Then get the accepted tokens from the contract
    let transaction_receipt = match app
        .proxy_web3_rpc(
            authorization.clone(),
            JsonRpcRequestEnum::Single(JsonRpcRequest {
                method: "eth_getTransactionReceipt".to_owned(),
                params: Some(serde_json::Value::Array(vec![serde_json::Value::String(
                    format!("0x{}", hex::encode(tx_hash)),
                )])),
                ..Default::default()
            }),
        )
        .await?
        .0
    {
        JsonRpcForwardedResponseEnum::Single(response) => match response.result {
            Some(raw_result) => Ok(raw_result),
            None => Err(Web3ProxyError("Transaction Hash was not found!".to_owned())),
        },
        _ => Err(Web3ProxyError::BadRequest(
            "Transaction Hash was not found!",
        )),
    }?;

    let (transaction_receipt, accepted_token, decimals): (TransactionReceipt, Address, u64) =
        match app
            .balanced_rpcs
            .best_available_rpc(&authorization, None, &[], None, None)
            .await
        {
            Ok(OpenRequestResult::Handle(handle)) => {
                // TODO: Figure out how to pass the transaction hash as a parameter ...
                warn!(
                    "Params are: {:?}",
                    &vec![format!("0x{}", hex::encode(tx_hash))]
                );
                let tx_receipt = handle
                    .clone()
                    .request(
                        "eth_getTransactionReceipt",
                        &vec![format!("0x{}", hex::encode(tx_hash))],
                        Level::Trace.into(),
                        None,
                    )
                    .await
                    // TODO: What kind of error would be here
                    .map_err(|err| Web3ProxyError::Anyhow(err.into()))?;

                // TODO: While you're at it, also grab the approved tokens from the llamanodes contract,
                let mut accepted_tokens_request_object: HashMap<String, serde_json::Value> =
                    HashMap::new();
                // We want to send a request to the contract
                accepted_tokens_request_object.insert(
                    "to".to_owned(),
                    serde_json::Value::String(app.config.deposit_contract.clone()),
                );
                // We then want to include the function that we want to call
                accepted_tokens_request_object.insert(
                    "data".to_owned(),
                    serde_json::Value::String(hex::encode(keccak256(
                        "get_approved_tokens()".as_bytes(),
                    ))),
                );
                let accepted_token = (&handle)
                    .request(
                        "eth_call",
                        &accepted_tokens_request_object,
                        Level::Trace.into(),
                        None,
                    )
                    .await
                    // TODO: What kind of error would be here
                    .map_err(|err| Web3ProxyError::Anyhow(err.into()))?;

                warn!("Accepted tokens are: {:?}", accepted_token);

                // and the decimals for the approved ERC20 token
                let mut token_decimals_request_object: HashMap<String, serde_json::Value> =
                    HashMap::new();
                // We want to send a request to the contract
                token_decimals_request_object.insert(
                    "to".to_owned(),
                    serde_json::Value::String(hex::encode(accepted_token)),
                );
                // We then want to include the function that we want to call
                token_decimals_request_object.insert(
                    "data".to_owned(),
                    serde_json::Value::String(hex::encode(keccak256("decimals()".as_bytes()))),
                );
                let decimals = (&handle)
                    .request(
                        "eth_call",
                        &token_decimals_request_object,
                        Level::Trace.into(),
                        None,
                    )
                    .await
                    // TODO: What kind of error would be here
                    .map_err(|err| Web3ProxyError::Anyhow(err.into()))?;

                warn!("Decimals are: {:?}", decimals);
                Ok((tx_receipt, accepted_token, decimals))
            }
            // TODO: Probably skip this case (?)
            Ok(_) => {
                // TODO: actually retry?
                // TODO: Is this the right error message?
                Err(Web3ProxyError::NoHandleReady)
            }
            Err(err) => {
                // TODO: Just skip this part until one item responds ...
                log::trace!(
                    "cancelled funneling transaction {} from: {:?}",
                    tx_hash,
                    err,
                );
                Err(err)
            }
        }?;
    debug!("Tx receipt: {:?}", transaction_receipt);

    // Go through all logs, this should prob capture it,
    // At least according to this SE logs are just concatenations of the underlying types (like a struct..)
    // https://ethereum.stackexchange.com/questions/87653/how-to-decode-log-event-of-my-transaction-log

    // Make sure there is only a single log within that transaction ...
    // I don't know how to best cover the case that there might be multiple logs inside

    for log in transaction_receipt.logs {
        warn!("Should be all from the deposit contract {:?}", log.address);
        if format!("0x{}", hex::encode(log.address)) != app.config.deposit_contract {
            warn!(
                "Wrong log address: {:?} {:?}",
                hex::encode(log.address),
                app.config.deposit_contract
            );
            continue;
        }

        // Get the topics out
        let token: Address = Address::from(log.topics.get(1).unwrap().to_owned());
        let recipient_account: Address = Address::from(log.topics.get(2).unwrap().to_owned());

        warn!("Check the data if we can decode it {:?}", log.data);
        warn!("Event log is {:?} {:?}", token, recipient_account);

        // Skip if no accepted token
        if token != accepted_token {
            warn!("Token is not accepted: {:?} != {:?}", token, accepted_token);
            continue;
        }

        // TODO: Will this work? Depends how logs are encoded
        // recipient_account, token,
        let amount: U256 = match ethers::abi::decode(
            &[
                // ParamType::Address,
                // ParamType::Address,
                ParamType::Uint(256usize),
            ],
            &log.data,
        ) {
            Ok(tpl) => Ok(tpl
                .get(0)
                .unwrap()
                .clone()
                .into_uint()
                .context("Could not decode amount")?),
            Err(err) => Err(Web3ProxyError::BadRequest(format!(
                "Log could not be decoded: {:?}",
                err
            ))),
        }?;

        warn!(
            "Coded items are: {:?} {:?} {:?}",
            hex::encode(recipient_account),
            hex::encode(token),
            amount
        );
        // warn!("Recipient account is: ")

        warn!("Recipient address is: {:?}", recipient_account);
        warn!("Recipient address is: {:?}", recipient_account.encode());

        // First, find all users ...
        let all_users = user::Entity::find().all(db_replica.conn()).await?;
        warn!("All users are: {:?}", all_users);

        // Encoding is inefficient, revisit later
        let recipient = match user::Entity::find()
            .filter(user::Column::Address.eq(&recipient_account.encode()[12..]))
            .one(db_replica.conn())
            .await?
        {
            Some(x) => Ok(x),
            None => Err(Web3ProxyError::BadRequest(
                "The user must have signed up first. They are currently not signed up!".to_string(),
            )),
        }?;

        let status_code: StatusCode;
        let response_json: serde_json::Value;

        // For now we only accept stablecoins
        // And we hardcode the peg (later we would have to depeg this, for example
        // 1$ = Decimal(1) for any stablecoin
        // TODO: Let's assume that people don't buy too much at _once_
        let amount = Decimal::from(amount.as_u128()) / (Decimal::from(10 ^ decimals));

        // Check if the item is in the database. If it is not, then add it into the database
        let user_balance = balance::Entity::find()
            .filter(balance::Column::UserId.eq(recipient.id))
            .one(&db_conn)
            .await?;
        debug!("User balance is: {:?}", user_balance);

        let txn = db_conn.begin().await?;
        match user_balance {
            Some(user_balance) => {
                let balance_plus_amount = user_balance.available_balance + amount;
                debug!("New user balance is: {:?}", balance_plus_amount);
                // Update the entry, adding the balance
                let mut user_balance = user_balance.into_active_model();
                user_balance.available_balance = sea_orm::Set(balance_plus_amount);
                debug!("New user balance model is: {:?}", user_balance);
                user_balance.save(&txn).await?;
                // txn.commit().await?;
                // user_balance
            }
            None => {
                // Create the entry with the respective balance
                let user_balance = balance::ActiveModel {
                    available_balance: sea_orm::ActiveValue::Set(amount),
                    // used_balance: sea_orm::ActiveValue::Set(0),
                    user_id: sea_orm::ActiveValue::Set(recipient.id),
                    ..Default::default()
                };
                debug!("New user balance model is: {:?}", user_balance);
                user_balance.save(&txn).await?;
                // txn.commit().await?;
                // user_balance // .try_into_model().unwrap()
            }
        };
        debug!("Setting tx_hash: {:?}", tx_hash);
        let receipt = increase_balance_receipt::ActiveModel {
            tx_hash: sea_orm::ActiveValue::Set(hex::encode(tx_hash)),
            ..Default::default()
        };
        receipt.save(&txn).await?;
        txn.commit().await?;
        debug!("Submitted saving");

        // Can return here
        debug!("Returning response");
        let response = (
            StatusCode::CREATED,
            Json(json!({
                "tx_hash": tx_hash,
                "amount": amount
            })),
        )
            .into_response();
        // Return early if the log was added
        return Ok(response.into());
    }

    Err(Web3ProxyError::BadRequest(
        "No such transaction was found, or token is not supported!".to_string(),
    ))
}
