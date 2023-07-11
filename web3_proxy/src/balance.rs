use crate::errors::{Web3ProxyErrorContext, Web3ProxyResult};
use entities::{
    admin_increase_balance_receipt, increase_on_chain_balance_receipt, referee, referrer,
    rpc_accounting_v2, rpc_key, stripe_increase_balance_receipt,
};
use migration::sea_orm::prelude::Decimal;
use migration::sea_orm::DbConn;
use migration::sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QuerySelect};
use migration::{Func, SimpleExpr};
use serde::ser::SerializeStruct;
use serde::Serialize;

/// Implements the balance getter which combines data from several tables
#[derive(Clone, Debug, Default)]
pub struct Balance {
    pub admin_deposits: Decimal,
    pub chain_deposits: Decimal,
    pub referal_bonus: Decimal,
    pub one_time_referee_bonus: Decimal,
    pub stripe_deposits: Decimal,
    pub total_spent: Decimal,
    pub total_spent_paid_credits: Decimal,
    pub user_id: u64,
}

impl Serialize for Balance {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("balance", 12)?;

        state.serialize_field("admin_deposits", &self.admin_deposits)?;
        state.serialize_field("chain_deposits", &self.chain_deposits)?;
        state.serialize_field("referal_bonus", &self.referal_bonus)?;
        state.serialize_field("one_time_referee_bonus", &self.one_time_referee_bonus)?;
        state.serialize_field("stripe_deposits", &self.stripe_deposits)?;
        state.serialize_field("total_spent", &self.total_spent)?;
        state.serialize_field("total_spent_paid_credits", &self.total_spent_paid_credits)?;
        state.serialize_field("user_id", &self.user_id)?;

        state.serialize_field("active_premium", &self.active_premium())?;
        state.serialize_field("was_ever_premium", &self.was_ever_premium())?;
        state.serialize_field("balance", &self.remaining())?;
        state.serialize_field("total_deposits", &self.total_deposits())?;

        state.end()
    }
}

impl Balance {
    pub fn active_premium(&self) -> bool {
        self.was_ever_premium() && self.total_deposits() > self.total_spent_paid_credits
    }

    pub fn was_ever_premium(&self) -> bool {
        self.user_id != 0 && self.total_deposits() >= Decimal::from(10)
    }

    pub fn remaining(&self) -> Decimal {
        self.total_deposits() - self.total_spent_paid_credits
    }

    pub fn total_deposits(&self) -> Decimal {
        self.admin_deposits
            + self.chain_deposits
            + self.referal_bonus
            + self.one_time_referee_bonus
            + self.stripe_deposits
    }

    /// TODO: do this with a single db query
    pub async fn try_from_db(db_conn: &DbConn, user_id: u64) -> Web3ProxyResult<Option<Self>> {
        // Return early if user_id == 0
        if user_id == 0 {
            return Ok(None);
        }

        let (admin_deposits,) = admin_increase_balance_receipt::Entity::find()
            .select_only()
            .column_as(
                SimpleExpr::from(Func::coalesce([
                    admin_increase_balance_receipt::Column::Amount.sum(),
                    0.into(),
                ])),
                "admin_deposits",
            )
            .filter(admin_increase_balance_receipt::Column::DepositToUserId.eq(user_id))
            .into_tuple()
            .one(db_conn)
            .await
            .web3_context("fetching admin deposits")?
            .unwrap_or_default();

        let (chain_deposits,) = increase_on_chain_balance_receipt::Entity::find()
            .select_only()
            .column_as(
                SimpleExpr::from(Func::coalesce([
                    increase_on_chain_balance_receipt::Column::Amount.sum(),
                    0.into(),
                ])),
                "chain_deposits",
            )
            .filter(increase_on_chain_balance_receipt::Column::DepositToUserId.eq(user_id))
            .into_tuple()
            .one(db_conn)
            .await
            .web3_context("fetching chain deposits")?
            .unwrap_or_default();

        let (stripe_deposits,) = stripe_increase_balance_receipt::Entity::find()
            .select_only()
            .column_as(
                SimpleExpr::from(Func::coalesce([
                    stripe_increase_balance_receipt::Column::Amount.sum(),
                    0.into(),
                ])),
                "stripe_deposits",
            )
            .filter(stripe_increase_balance_receipt::Column::DepositToUserId.eq(user_id))
            .into_tuple()
            .one(db_conn)
            .await
            .web3_context("fetching stripe deposits")?
            .unwrap_or_default();

        let (total_spent_paid_credits, total_spent) = rpc_accounting_v2::Entity::find()
            .select_only()
            .column_as(
                SimpleExpr::from(Func::coalesce([
                    rpc_accounting_v2::Column::SumCreditsUsed.sum(),
                    0.into(),
                ])),
                "total_spent_paid_credits",
            )
            .column_as(
                SimpleExpr::from(Func::coalesce([
                    rpc_accounting_v2::Column::SumInclFreeCreditsUsed.sum(),
                    0.into(),
                ])),
                "total_spent",
            )
            .inner_join(rpc_key::Entity)
            // .filter(rpc_key::Column::Id.eq(rpc_accounting_v2::Column::RpcKeyId))  // TODO: i think the inner_join function handles this
            .filter(rpc_key::Column::UserId.eq(user_id))
            .into_tuple()
            .one(db_conn)
            .await
            .web3_context("fetching total_spent_paid_credits and total_spent")?
            .unwrap_or_default();

        let one_time_referee_bonus = referee::Entity::find()
            .select_only()
            .column_as(
                referee::Column::OneTimeBonusAppliedForReferee,
                "one_time_bonus_applied_for_referee",
            )
            .filter(referee::Column::UserId.eq(user_id))
            .into_tuple()
            .one(db_conn)
            .await
            .web3_context("fetching one time referee bonus")?
            .unwrap_or_default();

        let referal_bonus = referee::Entity::find()
            .select_only()
            .column_as(
                SimpleExpr::from(Func::coalesce([
                    referee::Column::CreditsAppliedForReferrer.sum(),
                    0.into(),
                ])),
                "credits_applied_for_referrer",
            )
            .inner_join(referrer::Entity)
            .filter(referrer::Column::UserId.eq(user_id))
            .into_tuple()
            .one(db_conn)
            .await
            .web3_context("fetching referal bonus")?
            .unwrap_or_default();

        let balance = Self {
            admin_deposits,
            chain_deposits,
            referal_bonus,
            one_time_referee_bonus,
            stripe_deposits,
            total_spent,
            total_spent_paid_credits,
            user_id,
        };

        // Return None if there is no entry
        Ok(Some(balance))
    }
}
