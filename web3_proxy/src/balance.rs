use crate::errors::{Web3ProxyResponse, Web3ProxyResult};
use migration::sea_orm::{
    DbBackend, DbConn, FromQueryResult, JsonValue, QueryResult, SqlxMySqlPoolConnection, Statement,
};
use migration::{sea_orm, ConnectionTrait};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::num::NonZeroU64;
use tracing::info;

/// Implements the balance getter
#[derive(Clone, Debug, Default, Serialize, Deserialize, FromQueryResult)]
pub struct Balance {
    pub user_id: u64,
    pub total_spent_paid_credits: Decimal,
    pub total_spent: Decimal,
    pub total_deposits: Decimal,
}

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

    let raw_sql = r#"
        SELECT
            user.id AS user_id,
            COALESCE(SUM(admin_receipt.amount), 0) + COALESCE(SUM(chain_receipt.amount), 0) + COALESCE(SUM(stripe_receipt.amount), 0) + COALESCE(SUM(referee.one_time_bonus_applied_for_referee), 0) + COALESCE(referrer_bonus.total_bonus, 0) AS total_deposits,
            COALESCE(SUM(accounting.sum_credits_used), 0) AS total_spent_including_free_tier,
            COALESCE(SUM(accounting.sum_incl_free_credits_used), 0) AS total_spent_outside_free_tier
        FROM
            user
                LEFT JOIN
            admin_increase_balance_receipt AS admin_receipt ON user.id = admin_receipt.deposit_to_user_id
                LEFT JOIN
            increase_on_chain_balance_receipt AS chain_receipt ON user.id = chain_receipt.deposit_to_user_id
                LEFT JOIN
            stripe_increase_balance_receipt AS stripe_receipt ON user.id = stripe_receipt.deposit_to_user_id
                LEFT JOIN
            referee ON user.id = referee.user_id
                LEFT JOIN
            (SELECT referrer.user_id, SUM(referee.credits_applied_for_referrer) AS total_bonus
             FROM referrer
                      JOIN referee ON referrer.id = referee.used_referral_code
             GROUP BY referrer.user_id) AS referrer_bonus ON user.id = referrer_bonus.user_id
                LEFT JOIN
            rpc_key ON user.id = rpc_key.user_id
                LEFT JOIN
            rpc_accounting_v2 AS accounting ON rpc_key.id = accounting.rpc_key_id
                LEFT JOIN
            user_tier ON user.user_tier_id = user_tier.id
                WHERE
            user.id = $1;
    "#;

    let balance: Balance = match Balance::find_by_statement(Statement::from_sql_and_values(
        DbBackend::MySql,
        raw_sql,
        [user_id.into()],
    ))
    .one(db_conn)
    .await?
    {
        None => return Ok(None),
        Some(x) => x,
    };

    // Return None if there is no entry
    Ok(Some(balance))
}
