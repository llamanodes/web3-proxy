// TODO: This one does not need to be Authorized with a bearer token
// Anyone can call this basically (Up to some rate-limit so we dont get DDOS'ed)
// #[debug_handle]
// pub async fn user_submit_transaction(
//     Extension(app): Extension<Arc<Web3ProxyApp>>,
//     // TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>
//     Query(mut params): Query<HashMap<String, String>>
// ) -> FrontendResult {
//
//     // Get the tx-hash that was submitted ... (and make sure to add it to the database)
//     // Gotta make sure capitalization is correct (i.e. lowercase, with leading 0 for example, or not)
//     let tx_hash: String = params
//         .remove("tx_hash")
//         // TODO: map_err so this becomes a 500. routing must be bad
//         .ok_or(
//         FrontendErrorResponse::StatusCode(
//                 StatusCode::BAD_REQUEST,
//                 "You called the tx endpoint, but have not provided a tx_hash".to_string(),
//                 None,
//             )
//         )?;
//
//
//     // Go through RpcRequests
//     let db_replica = app.db_replica().context("Getting database connection")?;
//     let tx = rpc_request::Entity::find()
//         .filter(rpc_request::Column::TxHash.eq(tx_hash))
//         .one(db_replica.conn())
//         .await?
//         .ok_or(
//             FrontendErrorResponse::StatusCode(
//                 StatusCode::BAD_REQUEST,
//                 "No response with this tx hash was found".to_string(),
//                 None,
//             )
//         )?;
//
//     // Find the user that the credit belongs to
//     let user_id = tx.user_id;
//
//     // Make an RPC request, and check how many confirmations this transaction has received
//     // get the head block_number
//     query_transaction_status
//
//     let latest_block_number = app.balanced_rpcs.head_block_num().ok_or(
//         FrontendErrorResponse::StatusCode(
//             StatusCode::SERVICE_UNAVAILABLE,
//             "Latest block number was not received, RPCs don't seem to be synced".to_string(),
//             None,
//         )
//     )? as u64;
//
//     // Do the same, and get the block number of this tx-hash
//     let tx_block_number = app.balanced_rpcs.query_transaction_status()
//
//     // TODO: Best would be to make a batch-request to all items in the response-queue (if they were not processed yet)
//     // and then go by that
//
//
//     // Basically get the block where this transaction has occurred
//     // If there are 6 blocks ever since, consider it done, and subtract the credits from the user ...
//
//     // Remove credits, and remove the tx from the db if it is older than 1 minute (accounting for block-limits)
//     let db_conn = app.db_conn().context("deleting expired pending logins requires a db")?;
// }