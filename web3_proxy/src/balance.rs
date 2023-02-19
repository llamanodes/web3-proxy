use migration::sea_orm::DatabaseConnection;
use crate::frontend::errors::FrontendErrorResponse;

/// Implements logic to include balances
///
/// Adds an entry to the `spent` table (rpc_requests)
/// Then subtracts the user balance from the `balance` table
pub async fn update_balance_and_spend(user_id: u64, db_conn: &DatabaseConnection, amount_delta: u64) -> Result<u64, FrontendErrorResponse> {

    // Add a new entry into the rpc endpoint ...
    todo!()
}