//! Store "stats" in a database for billing and a different database for graphing
//! TODO: move some of these structs/functions into their own file?
mod stat_buffer;

pub mod db_queries;
pub mod influxdb_queries;

use self::stat_buffer::BufferedRpcQueryStats;
use crate::caches::{RpcSecretKeyCache, UserBalanceCache};
use crate::compute_units::ComputeUnit;
use crate::errors::{Web3ProxyError, Web3ProxyResult};
use crate::frontend::authorization::{Authorization, Balance, RequestMetadata};
use crate::rpcs::one::Web3Rpc;
use anyhow::{anyhow, Context};
use axum::headers::Origin;
use chrono::{DateTime, Months, TimeZone, Utc};
use derive_more::From;
use entities::{balance, referee, referrer, rpc_accounting_v2, rpc_key};
use futures::TryFutureExt;
use influxdb2::models::DataPoint;
use migration::sea_orm::prelude::Decimal;
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, DatabaseConnection, DbConn, EntityTrait, QueryFilter,
    TransactionTrait,
};
use migration::sea_orm::{DatabaseTransaction, QuerySelect};
use migration::{Expr, LockType, OnConflict};
use num_traits::{ToPrimitive, Zero};
use parking_lot::{Mutex, RwLock};
use rdkafka::admin::ConfigSource::Default;
use sentry::User;
use std::borrow::Cow;
use std::convert::Infallible;
use std::default::Default;
use std::mem;
use std::num::NonZeroU64;
use std::str::FromStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tracing::{error, info, trace};

pub use stat_buffer::{SpawnedStatBuffer, StatBuffer};

#[derive(Debug, PartialEq, Eq)]
pub enum StatType {
    Aggregated,
    Detailed,
}

pub type BackendRequests = Mutex<Vec<Arc<Web3Rpc>>>;

/// TODO: better name? RpcQueryStatBuilder?
#[derive(Clone, Debug)]
pub struct RpcQueryStats {
    pub chain_id: u64,
    pub authorization: Arc<Authorization>,
    pub method: Cow<'static, str>,
    pub archive_request: bool,
    pub error_response: bool,
    pub request_bytes: u64,
    /// if backend_requests is 0, there was a cache_hit
    /// no need to track frontend_request on this. a RpcQueryStats always represents one frontend request
    pub backend_rpcs_used: Vec<Arc<Web3Rpc>>,
    pub response_bytes: u64,
    pub response_millis: u64,
    pub response_timestamp: i64,
    /// The cost of the query in USD
    /// If the user is on a free tier, this is still calculated so we know how much we are giving away.
    pub compute_unit_cost: Decimal,
    /// If the request is invalid or received a jsonrpc error response (excluding reverts)
    pub user_error_response: bool,
}

#[derive(Clone, Debug, From, Hash, PartialEq, Eq)]
pub struct RpcQueryKey {
    /// unix epoch time.
    /// for the time series db, this is (close to) the time that the response was sent.
    /// for the account database, this is rounded to the week.
    response_timestamp: i64,
    /// true if an archive server was needed to serve the request.
    archive_needed: bool,
    /// true if the response was some sort of application error.
    error_response: bool,
    /// true if the response was some sort of JSONRPC error.
    user_error_response: bool,
    /// the rpc method used.
    method: Cow<'static, str>,
    /// origin tracking **was** opt-in. Now, it is always "None"
    origin: Option<Origin>,
    /// None if the public url was used.
    rpc_secret_key_id: Option<NonZeroU64>,
    /// None if the public url was used.
    rpc_key_user_id: Option<NonZeroU64>,
}

/// round the unix epoch time to the start of a period
fn round_timestamp(timestamp: i64, period_seconds: i64) -> i64 {
    timestamp / period_seconds * period_seconds
}

impl RpcQueryStats {
    /// rpc keys can opt into multiple levels of tracking.
    /// we always need enough to handle billing, so the "none" level was changed to "minimal" tracking.
    /// This "accounting_key" is used in the relational database.
    /// anonymous users are also saved in the relational database so that the host can do their own cost accounting.
    fn accounting_key(&self, period_seconds: i64) -> RpcQueryKey {
        let response_timestamp = round_timestamp(self.response_timestamp, period_seconds);

        let rpc_secret_key_id = self.authorization.checks.rpc_secret_key_id;

        let method = self.method.clone();

        // we used to optionally store origin, but wallets don't set it, so its almost always None
        let origin = None;

        // user_error_response is always set to false because we don't bother tracking this in the database
        let user_error_response = false;

        // Depending on method, add some arithmetic around calculating credits_used
        // I think balance should not go here, this looks more like a key thingy
        RpcQueryKey {
            response_timestamp,
            archive_needed: self.archive_request,
            error_response: self.error_response,
            method,
            rpc_secret_key_id,
            rpc_key_user_id: self.authorization.checks.user_id.try_into().ok(),
            origin,
            user_error_response,
        }
    }

    /// all rpc keys are aggregated in the global stats
    /// TODO: should we store "anon" or "registered" as a key just to be able to split graphs?
    fn global_timeseries_key(&self) -> RpcQueryKey {
        // we include the method because that can be helpful for predicting load
        let method = self.method.clone();
        // we don't store origin in the timeseries db. its only used for optional accounting
        let origin = None;
        // everyone gets grouped together
        let rpc_secret_key_id = None;

        RpcQueryKey {
            response_timestamp: self.response_timestamp,
            archive_needed: self.archive_request,
            error_response: self.error_response,
            method,
            rpc_secret_key_id,
            rpc_key_user_id: self.authorization.checks.user_id.try_into().ok(),
            user_error_response: self.user_error_response,
            origin,
        }
    }

    /// stats for a single key
    fn owned_timeseries_key(&self) -> Option<RpcQueryKey> {
        // we don't store origin in the timeseries db. its only optionaly used for accounting
        let origin = None;

        let method = self.method.clone();

        let key = RpcQueryKey {
            response_timestamp: self.response_timestamp,
            archive_needed: self.archive_request,
            error_response: self.error_response,
            method,
            rpc_secret_key_id: self.authorization.checks.rpc_secret_key_id,
            rpc_key_user_id: self.authorization.checks.user_id.try_into().ok(),
            user_error_response: self.user_error_response,
            origin,
        };

        Some(key)
    }
}

#[derive(Debug, Default)]
struct Deltas {
    balance_spent_including_free_credits: Decimal,
    balance_spent_excluding_free_credits: Decimal,
    apply_usage_bonus_to_request_sender: bool,
    usage_bonus_to_request_sender_through_referral: Decimal,

    bonus_to_referrer: Decimal,
}

/// A stat that we aggregate and then store in a database.
/// For now there is just one, but I think there might be others later
#[derive(Debug, From)]
pub enum AppStat {
    RpcQuery(RpcQueryStats),
}

// TODO: move to stat_buffer.rs?
impl BufferedRpcQueryStats {
    fn add(&mut self, stat: RpcQueryStats) {
        // a stat always come from just 1 frontend request
        self.frontend_requests += 1;

        // TODO: is this always okay? is it true that each backend rpc will only be queried once per request? i think so
        let num_backend_rpcs_used = stat.backend_rpcs_used.len() as u64;

        if num_backend_rpcs_used == 0 {
            // no backend request. cache hit!
            self.cache_hits += 1;
        } else {
            // backend requests! cache miss!
            self.cache_misses += 1;

            // a single frontend request might have multiple backend requests
            self.backend_requests += num_backend_rpcs_used;
        }

        self.sum_request_bytes += stat.request_bytes;
        self.sum_response_bytes += stat.response_bytes;
        self.sum_response_millis += stat.response_millis;
        self.sum_credits_used += stat.compute_unit_cost;

        let latest_balance = stat.authorization.checks.latest_balance.read();
        self.approximate_latest_balance_for_influx = latest_balance.clone();
    }

    async fn _save_db_stats(
        &self,
        chain_id: u64,
        db_conn: &DatabaseConnection,
        key: &RpcQueryKey,
        user_balance: Decimal,
    ) -> Web3ProxyResult<()> {
        let period_datetime = Utc.timestamp_opt(key.response_timestamp, 0).unwrap();

        // Because reading the balance and updating the stats here is not atomically locked, this may lead to a negative balance
        // This negative balance shouldn't be large tough
        let paid_credits_used = if self.sum_credits_used >= user_balance {
            user_balance
        } else {
            self.sum_credits_used
        };

        // =============================== //
        //       UPDATE STATISTICS         //
        // =============================== //
        let accounting_entry = rpc_accounting_v2::ActiveModel {
            id: sea_orm::NotSet,
            rpc_key_id: sea_orm::Set(key.rpc_secret_key_id.map(Into::into)),
            chain_id: sea_orm::Set(chain_id),
            period_datetime: sea_orm::Set(period_datetime),
            archive_needed: sea_orm::Set(key.archive_needed),
            error_response: sea_orm::Set(key.error_response),
            frontend_requests: sea_orm::Set(self.frontend_requests),
            backend_requests: sea_orm::Set(self.backend_requests),
            backend_retries: sea_orm::Set(self.backend_retries),
            no_servers: sea_orm::Set(self.no_servers),
            cache_misses: sea_orm::Set(self.cache_misses),
            cache_hits: sea_orm::Set(self.cache_hits),
            sum_request_bytes: sea_orm::Set(self.sum_request_bytes),
            sum_response_millis: sea_orm::Set(self.sum_response_millis),
            sum_response_bytes: sea_orm::Set(self.sum_response_bytes),
            sum_credits_used: sea_orm::Set(paid_credits_used),
            sum_free_credits_used: sea_orm::Set(self.sum_credits_used),
        };

        rpc_accounting_v2::Entity::insert(accounting_entry)
            .on_conflict(
                OnConflict::new()
                    .values([
                        (
                            rpc_accounting_v2::Column::FrontendRequests,
                            Expr::col(rpc_accounting_v2::Column::FrontendRequests)
                                .add(self.frontend_requests),
                        ),
                        (
                            rpc_accounting_v2::Column::BackendRequests,
                            Expr::col(rpc_accounting_v2::Column::BackendRequests)
                                .add(self.backend_requests),
                        ),
                        (
                            rpc_accounting_v2::Column::BackendRetries,
                            Expr::col(rpc_accounting_v2::Column::BackendRetries)
                                .add(self.backend_retries),
                        ),
                        (
                            rpc_accounting_v2::Column::NoServers,
                            Expr::col(rpc_accounting_v2::Column::NoServers).add(self.no_servers),
                        ),
                        (
                            rpc_accounting_v2::Column::CacheMisses,
                            Expr::col(rpc_accounting_v2::Column::CacheMisses)
                                .add(self.cache_misses),
                        ),
                        (
                            rpc_accounting_v2::Column::CacheHits,
                            Expr::col(rpc_accounting_v2::Column::CacheHits).add(self.cache_hits),
                        ),
                        (
                            rpc_accounting_v2::Column::SumRequestBytes,
                            Expr::col(rpc_accounting_v2::Column::SumRequestBytes)
                                .add(self.sum_request_bytes),
                        ),
                        (
                            rpc_accounting_v2::Column::SumResponseMillis,
                            Expr::col(rpc_accounting_v2::Column::SumResponseMillis)
                                .add(self.sum_response_millis),
                        ),
                        (
                            rpc_accounting_v2::Column::SumResponseBytes,
                            Expr::col(rpc_accounting_v2::Column::SumResponseBytes)
                                .add(self.sum_response_bytes),
                        ),
                        (
                            rpc_accounting_v2::Column::SumFreeCreditsUsed,
                            Expr::col(rpc_accounting_v2::Column::SumFreeCreditsUsed)
                                .add(self.paid_credits_used),
                        ),
                        (
                            rpc_accounting_v2::Column::SumCreditsUsed,
                            Expr::col(rpc_accounting_v2::Column::SumCreditsUsed)
                                .add(self.sum_credits_used),
                        ),
                    ])
                    .to_owned(),
            )
            .exec(db_conn)
            .await?;

        Ok(())
    }

    async fn _compute_balance_deltas(
        &self,
        sender_balance: balance::Model,
        referral_objects: Option<(referee::Model, referrer::Model)>,
    ) -> Web3ProxyResult<(Deltas, Option<(referee::Model, referrer::Model)>)> {
        // Calculate Balance Only
        let mut deltas = Deltas::default();

        // Calculate a bunch using referrals as well
        if let Some((referral_entity, referrer_code_entity)) = referral_objects {
            deltas.apply_usage_bonus_to_request_sender =
                referral_entity.credits_applied_for_referee.is_zero();

            // Calculate if we are above the usage threshold, and apply a bonus
            // Optimally we would read this from the balance, but if we do it like this, we only have to lock a single table (much safer w.r.t. deadlocks)
            // referral_entity.credits_applied_for_referrer * (Decimal::from(10) checks (atomically using this table only), whether the user has brought in >$100 to the referer
            // In this case, the sender receives $100 as a bonus / gift
            // Apply a 10$ bonus onto the user, if the user has spent 100$
            trace!(
                "Were credits applied so far? {:?} {:?}",
                referral_entity.credits_applied_for_referee,
                referral_entity.credits_applied_for_referee
            );
            trace!(
                "Credits applied for referrer so far? {:?}",
                referral_entity.credits_applied_for_referrer
            );
            trace!("Sum credits used? {:?}", self.sum_credits_used);
            if referral_entity.credits_applied_for_referee.is_zero()
                && (referral_entity.credits_applied_for_referrer * (Decimal::from(10))
                    + self.sum_credits_used)
                    >= Decimal::from(100)
            {
                trace!("Adding sender bonus balance");
                deltas.usage_bonus_to_request_sender_through_referral = Decimal::from(10);
                deltas.apply_usage_bonus_to_request_sender = true;
            }

            // Calculate how much the referrer should get, limited to the last 12 months
            // Apply 10% of the used balance as a bonus if applicable
            let now = Utc::now();
            let valid_until = DateTime::<Utc>::from_utc(referral_entity.referral_start_date, Utc)
                + Months::new(12);

            if now <= valid_until {
                deltas.bonus_to_referrer += self.sum_credits_used / Decimal::new(10, 0);
            }

            // Duplicate code, I should fix this later ...
            let user_balance = sender_balance.total_deposits
                - sender_balance.total_spent_outside_free_tier
                + deltas.usage_bonus_to_request_sender_through_referral;

            // Split up the component of into how much of the paid component was used, and how much of the free component was used (anything after "balance")
            if user_balance - self.sum_credits_used >= Decimal::from(0) {
                deltas.balance_spent_including_free_credits = self.sum_credits_used;
                deltas.balance_spent_excluding_free_credits = self.sum_credits_used;
            } else {
                deltas.balance_spent_including_free_credits = self.sum_credits_used;
                deltas.balance_spent_excluding_free_credits = user_balance;
            }

            Ok((deltas, Some((referral_entity, referrer_code_entity))))
        } else {
            let user_balance = sender_balance.total_deposits
                - sender_balance.total_spent_outside_free_tier
                + deltas.usage_bonus_to_request_sender_through_referral;

            // Split up the component of into how much of the paid component was used, and how much of the free component was used (anything after "balance")
            if user_balance - self.sum_credits_used >= Decimal::from(0) {
                deltas.balance_spent_including_free_credits = self.sum_credits_used;
                deltas.balance_spent_excluding_free_credits = self.sum_credits_used;
            } else {
                deltas.balance_spent_including_free_credits = self.sum_credits_used;
                deltas.balance_spent_excluding_free_credits = user_balance;
            }

            Ok((deltas, None))
        }
    }

    // TODO: This is basically a duplicate with the balance_checks, except the database
    // TODO: Please refactor this. Also there are small differences, like the Error is 0
    async fn _get_user_balance(
        &self,
        user_id: u64,
        user_balance_cache: &UserBalanceCache,
        txn: &DbConn,
        db_conn: &DbConn,
    ) -> Arc<RwLock<Balance>> {
        loop {
            match NonZeroU64::try_from(sender_rpc_entity.user_id) {
                Ok(user_id) => {
                    match user_balance_cache.try_get_with(&user_id, async move {}) {
                        Some(x) => {
                            return x;
                        }
                        // If not in cache, nothing to update
                        None => {
                            let x = balance::Entity::find()
                                .filter(balance::Column::UserId.eq(user_id))
                                .one(&txn)
                                .await?
                                .unwrap_or_else({
                                    error!(
                                        "It seems like this user has no balance entry {:?}",
                                        user_id
                                    );
                                    Decimal::zero()
                                });
                            let x = Balance {
                                total_deposit: x.total_deposits,
                                total_spend: x.total_spent_outside_free_tier,
                            };
                            // Insert it if not
                            x
                            // Also insert this into the cache
                        }
                    }
                }
                Err(_) => Decimal::zero(),
            }
        }
    }

    /// Save all referral-based objects in the database
    async fn _update_balances_in_db(
        &self,
        deltas: &Deltas,
        txn: &DatabaseTransaction,
        referral_objects: &Option<(referee::Model, referrer::Model)>,
    ) -> Web3ProxyResult<()> {
        // In any case, add to the balance
        trace!(
            "Delta is: {:?} from credits used {:?}",
            deltas,
            self.sum_credits_used
        );

        // If there is a referrer, do the referrer_entry updates
        if let Some((mut referral_entity, referrer_code_entity)) = referral_objects {
            // First check if the one-time bonus applies to the request-sender
            // first of all check if credits should be applied to the sender itself (once he spent 100$)
            // If these two values are not equal, that means that we have not applied the bonus just yet.
            // In that case, we can apply the bonus just now.
            if referral_entity.credits_applied_for_referee.is_zero()
                && deltas.apply_usage_bonus_to_request_sender
            {
                // Also add a bonus to the sender (But this should already have been done with the above code!!)
                let new_referee_entity = referee::ActiveModel {
                    id: sea_orm::Unchanged(Default::default()),
                    credits_applied_for_referee: sea_orm::Set(
                        referral_entity.credits_applied_for_referee,
                    ),
                    credits_applied_for_referrer: sea_orm::Unchanged(Default::default()),
                    referral_start_date: sea_orm::Unchanged(Default::default()),
                    used_referral_code: sea_orm::Unchanged(Default::default()),
                    user_id: sea_orm::Unchanged(Default::default()),
                };
                // The resulting field will not be read again, so I will not try to turn the ActiveModel into a Model one
                new_referee_entity.save(txn).await?;
            }

            // If the bonus to the referrer is non-empty, also apply that
            if deltas.bonus_to_referrer > Decimal::from(0) {
                referee::Entity::insert(referee_entry)
                    .on_conflict(
                        OnConflict::new()
                            .values([(
                                // TODO Make it a "Set", add is hacky (but works ..)
                                referee::Column::CreditsAppliedForReferrer,
                                Expr::col(referee::Column::CreditsAppliedForReferrer)
                                    .add(deltas.bonus_to_referrer),
                            )])
                            .to_owned(),
                    )
                    .exec(txn)
                    .await?;
            }
        };
        Ok(())
    }

    /// Update & Invalidate cache if user is credits are low (premium downgrade condition)
    /// Reduce credits if there was no issue
    /// This is not atomic, so this may be an issue because it's not sequentially consistent across threads
    /// It is a good-enough approximation though, and if the TTL for the balance cache is low enough, this should be ok
    async fn _update_balance_in_cache(
        &self,
        deltas: &Deltas,
        db_conn: &DatabaseConnection,
        sender_rpc_entity: &rpc_key::Model,
        rpc_secret_key_cache: &RpcSecretKeyCache,
        user_balance_cache: &UserBalanceCache,
    ) -> Web3ProxyResult<()> {
        // ==================
        // Modify sender balance
        // ==================
        let user_id = NonZeroU64::try_from(sender_rpc_entity.user_id)
            .expect("database ids are always nonzero");

        // We don't do an get_or_insert, because technically we don't have the most up to date balance
        // Also let's keep things simple in terms of writing and getting. A single place writes it, multiple places can remove / poll it
        let latest_balance = match user_balance_cache.get(&user_id) {
            Some(x) => x,
            // If not in cache, nothing to update
            None => return Ok(()),
        };

        let (balance_before, latest_balance) = {
            let mut latest_balance = latest_balance.write();

            let balance_before = latest_balance.clone();

            // Now modify the balance
            latest_balance.total_deposit += deltas.usage_bonus_to_request_sender_through_referral;
            latest_balance.total_spend += deltas.balance_spent_including_free_credits;

            (balance_before, latest_balance.clone())
        };

        // we only start subtracting once the user is first upgraded to a premium user
        // consider the user premium if total_deposit > premium threshold
        // If the balance is getting low, clear the cache
        // TODO: configurable amount for "premium"
        // TODO: configurable amount for "low"
        // we check balance_before because this current request would have been handled with limits matching the balance at the start of the request
        if balance_before.total_deposit > Decimal::from(10)
            && latest_balance.remaining() <= Decimal::from(10)
        {
            let rpc_keys = rpc_key::Entity::find()
                .filter(rpc_key::Column::UserId.eq(sender_rpc_entity.user_id))
                .all(db_conn)
                .await?;

            // clear the user from the cache
            if let Ok(user_id) = NonZeroU64::try_from(sender_rpc_entity.user_id) {
                user_balance_cache.invalidate(&user_id).await;
            }

            // clear all keys owned by this user from the cache
            for rpc_key_entity in rpc_keys {
                rpc_secret_key_cache
                    .invalidate(&rpc_key_entity.secret_key.into())
                    .await;
            }
        }
        Ok(())
    }

    // TODO: take a db transaction instead so that we can batch?
    async fn save_db(
        self,
        chain_id: u64,
        db_conn: &DatabaseConnection,
        key: RpcQueryKey,
        rpc_secret_key_cache: &RpcSecretKeyCache,
        user_balance_cache: &UserBalanceCache,
    ) -> Web3ProxyResult<()> {
        // Sanity check, if we need to save stats
        if key.response_timestamp == 0 {
            return Err(Web3ProxyError::Anyhow(anyhow!(
                "no response_timestamp! This is a bug! {:?} {:?}",
                key,
                self
            )));
        }

        // Start a transaction
        let txn = db_conn.begin().await?;

        // Gathering cache and database rows
        let user_balance = self._get_user_balance(sender_rpc_entity.user_id).await?;

        // First of all, save the statistics to the database:
        self._save_db_stats(chain_id, &txn, &key, user_balance)
            .await?;

        // No need to continue if no credits were used
        if self.sum_credits_used == 0.into() {
            // write-lock is released
            txn.commit().await?;
            return Ok(());
        }

        match referee::Entity::find()
            .filter(referee::Column::UserId.eq(user_id))
            .find_also_related(referrer::Entity)
            .one(&txn)
            .await?
        {
            Some((referee, referrer)) => {

                // Get the balance for the referrer, see if they're premium or not

                // Compute all the deltas
            }
            None => None,
        };

        // Compute Changes in balance for user and referrer, incl. referral logic
        let (deltas, referral_objects): (Deltas, Option<(referee::Model, referrer::Model)>) = self
            ._compute_balance_deltas(_sender_balance, referral_objects)
            .await?;

        // Update balances in the database
        self._update_balances_in_db(&deltas, &txn, &referral_objects)
            .await?;

        // Finally commit the transaction in the database
        txn.commit()
            .await
            .context("Failed to update referral and balance updates")?;

        // Update balanaces in the cache.
        // do this after commiting the database so that invalidated caches definitely query commited data
        self._update_balance_in_cache(
            &deltas,
            db_conn,
            &sender_rpc_entity,
            rpc_secret_key_cache,
            user_balance_cache,
        )
        .await?;

        Ok(())
    }

    async fn build_timeseries_point(
        self,
        measurement: &str,
        chain_id: u64,
        key: RpcQueryKey,
    ) -> anyhow::Result<DataPoint> {
        let mut builder = DataPoint::builder(measurement);

        builder = builder.tag("chain_id", chain_id.to_string());

        if let Some(rpc_secret_key_id) = key.rpc_secret_key_id {
            builder = builder.tag("rpc_secret_key_id", rpc_secret_key_id.to_string());
        }

        builder = builder.tag("method", key.method);

        // Read the latest balance ...
        let remaining = self.approximate_latest_balance_for_influx.remaining();
        trace!("Remaining balance for influx is {:?}", remaining);

        builder = builder
            .tag("archive_needed", key.archive_needed.to_string())
            .tag("error_response", key.error_response.to_string())
            .tag("user_error_response", key.user_error_response.to_string())
            .field("frontend_requests", self.frontend_requests as i64)
            .field("backend_requests", self.backend_requests as i64)
            .field("no_servers", self.no_servers as i64)
            .field("cache_misses", self.cache_misses as i64)
            .field("cache_hits", self.cache_hits as i64)
            .field("sum_request_bytes", self.sum_request_bytes as i64)
            .field("sum_response_millis", self.sum_response_millis as i64)
            .field("sum_response_bytes", self.sum_response_bytes as i64)
            .field(
                "sum_credits_used",
                self.sum_credits_used
                    .to_f64()
                    .context("sum_credits_used is really (too) large")?,
            )
            .field(
                "balance",
                remaining
                    .to_f64()
                    .context("balance is really (too) large")?,
            );

        builder = builder.timestamp(key.response_timestamp);

        let point = builder.build()?;

        trace!("Datapoint saving to Influx is {:?}", point);

        Ok(point)
    }
}

impl TryFrom<RequestMetadata> for RpcQueryStats {
    type Error = Web3ProxyError;

    fn try_from(mut metadata: RequestMetadata) -> Result<Self, Self::Error> {
        let mut authorization = metadata.authorization.take();

        if authorization.is_none() {
            authorization = Some(Arc::new(Authorization::internal(None)?));
        }

        let authorization = authorization.expect("Authorization will always be set");

        let archive_request = metadata.archive_request.load(Ordering::Acquire);

        // TODO: do this without cloning. we can take their vec
        let backend_rpcs_used = metadata.backend_rpcs_used();

        let request_bytes = metadata.request_bytes as u64;
        let response_bytes = metadata.response_bytes.load(Ordering::Acquire);

        let mut error_response = metadata.error_response.load(Ordering::Acquire);
        let mut response_millis = metadata.response_millis.load(Ordering::Acquire);

        let user_error_response = metadata.user_error_response.load(Ordering::Acquire);

        let response_timestamp = match metadata.response_timestamp.load(Ordering::Acquire) {
            0 => {
                // no response timestamp!
                if !error_response {
                    // force error_response to true
                    // this can happen when a try operator escapes and metadata.add_response() isn't called
                    trace!(
                        "no response known, but no errors logged. investigate. {:?}",
                        metadata
                    );
                    error_response = true;
                }

                if response_millis == 0 {
                    // get something for millis even if it is a bit late
                    response_millis = metadata.start_instant.elapsed().as_millis() as u64
                }

                // no timestamp given. likely handling an error. set it to the current time
                Utc::now().timestamp()
            }
            x => x,
        };

        let cu = ComputeUnit::new(&metadata.method, metadata.chain_id, response_bytes);

        // TODO: get from config? a helper function? how should we pick this?
        let usd_per_cu = match metadata.chain_id {
            137 => Decimal::from_str("0.000000533333333333333"),
            _ => Decimal::from_str("0.000000400000000000000"),
        }?;
        info!("usd_per_cu");
        info!("{}", usd_per_cu);

        let cache_hit = !backend_rpcs_used.is_empty();

        let compute_unit_cost = cu.cost(archive_request, cache_hit, error_response, usd_per_cu);

        let method = mem::take(&mut metadata.method);

        let x = Self {
            archive_request,
            authorization,
            backend_rpcs_used,
            chain_id: metadata.chain_id,
            compute_unit_cost,
            error_response,
            method,
            request_bytes,
            response_bytes,
            response_millis,
            response_timestamp,
            user_error_response,
        };

        Ok(x)
    }
}
