use crate::errors::{Web3ProxyResponse, Web3ProxyResult};
use migration::sea_orm::{
    DbBackend, DbConn, JsonValue, SqlxMySqlPoolConnection, Statement, TryGetableMany,
};
use migration::ConnectionTrait;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::num::NonZeroU64;
use tracing::info;

/// Implements the balance getter
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Balance {
    pub user_id: i32,
    pub total_spent_paid_credits: Decimal,
    pub total_spent: Decimal,
    pub total_deposits: Decimal,
}

// TODO: Implement remaining
impl Balance {
    pub fn remaining(&self) -> Decimal {
        self.total_deposits - self.total_spent_paid_credits
    }
}

pub async fn get_balance_from_db(
    db_conn: &DbConn,
    user_id: u64,
) -> Web3ProxyResult<Option<Balance>> {
    // Return early if user_id == 0
    if user_id == 0 {
        return Ok(None);
    }

    // Make the SQL query
    // let balance: Vec<JsonValue> =
    //     JsonValue::find_by_statement::<MySqlConnection>(Statement::from_sql_and_values(
    //         db_conn.get_database_backend(),
    //         r#"SELECT "cake"."name" FROM "cake" GROUP BY "cake"."name"#,
    //         [],
    //     ))
    //     .all(db_conn)
    //     .await?;

    // let balance = match balance {
    //     None => return Ok(None),
    //     Some(x) => x,
    // };

    // info!("Balance is {:?}", balance);

    // Return None if there is no entry
    Ok(Some(Balance::default()))
}
