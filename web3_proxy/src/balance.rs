use migration::sea_orm::{DbBackend, DbConn, Statement, TryGetableMany};
use migration::ConnectionTrait;
use rust_decimal::Decimal;
use sea_orm::FromQueryResult;
use serde::{Deserialize, Serialize};
use std::num::NonZeroU64;

/// Implements the balance getter
#[derive(Clone, Debug, Default, Serialize, Deserialize, FromQueryResult)]
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

pub async fn get_balance_from_db(db_conn: &DbConn, user_id: u64) -> Option<Balance> {
    // Return early if user_id == 0
    if user_id == 0 {
        return None;
    }

    // Make the SQL query
    let balance: Option<Balance> = Balance::find_by_statement(Statement::from_sql_and_values(
        db_conn.get_database_backend(),
        r#"SELECT "cake"."name" FROM "cake" GROUP BY "cake"."name"#,
        [user_id],
    ))
    .one(&db_conn)
    .await?;

    // Return None if there is no entry
    todo!()
}
