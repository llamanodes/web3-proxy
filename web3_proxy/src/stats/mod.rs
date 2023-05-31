//! Store "stats" in a database for billing and a different database for graphing
//! TODO: move some of these structs/functions into their own file?
pub mod db_queries;
pub mod influxdb_queries;
mod stat_buffer;
pub use stat_buffer::{SpawnedStatBuffer, StatBuffer};
use std::cmp;

use crate::app::RpcSecretKeyCache;
use crate::frontend::authorization::{Authorization, RequestMetadata};
use crate::frontend::errors::{Web3ProxyError, Web3ProxyResult};
use crate::rpcs::one::Web3Rpc;
use anyhow::{anyhow, Context};
use axum::headers::Origin;
use chrono::{DateTime, Months, TimeZone, Utc};
use derive_more::From;
use entities::sea_orm_active_enums::TrackingLevel;
use entities::{balance, referee, referrer, rpc_accounting_v2, rpc_key, user};
use influxdb2::models::DataPoint;
use log::trace;
use migration::sea_orm::prelude::Decimal;
use migration::sea_orm::QuerySelect;
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, IntoActiveModel,
    QueryFilter, TransactionTrait,
};
use migration::{Expr, LockType, OnConflict};
use num_traits::ToPrimitive;
use parking_lot::Mutex;
use std::num::NonZeroU64;
use std::sync::atomic::{self, Ordering};
use std::sync::Arc;

use self::stat_buffer::BufferedRpcQueryStats;

#[derive(Debug, PartialEq, Eq)]
pub enum StatType {
    Aggregated,
    Detailed,
}

pub type BackendRequests = Mutex<Vec<Arc<Web3Rpc>>>;

/// TODO: better name? RpcQueryStatBuilder?
#[derive(Clone, Debug)]
pub struct RpcQueryStats {
    pub authorization: Arc<Authorization>,
    pub method: Option<String>,
    pub archive_request: bool,
    pub error_response: bool,
    pub request_bytes: u64,
    /// if backend_requests is 0, there was a cache_hit
    /// no need to track frontend_request on this. a RpcQueryStats always represents one frontend request
    pub backend_rpcs_used: Vec<Arc<Web3Rpc>>,
    pub response_bytes: u64,
    pub response_millis: u64,
    pub response_timestamp: i64,
    /// Credits used signifies how how much money was used up
    pub credits_used: Decimal,
}

#[derive(Clone, Debug, From, Hash, PartialEq, Eq)]
pub struct RpcQueryKey {
    /// unix epoch time
    /// for the time series db, this is (close to) the time that the response was sent
    /// for the account database, this is rounded to the week
    response_timestamp: i64,
    /// true if an archive server was needed to serve the request
    archive_needed: bool,
    /// true if the response was some sort of JSONRPC error
    error_response: bool,
    /// method tracking is opt-in
    method: Option<String>,
    /// origin tracking is opt-in
    origin: Option<Origin>,
    /// None if the public url was used
    rpc_secret_key_id: Option<NonZeroU64>,
}

/// round the unix epoch time to the start of a period
fn round_timestamp(timestamp: i64, period_seconds: i64) -> i64 {
    timestamp / period_seconds * period_seconds
}

impl RpcQueryStats {
    /// rpc keys can opt into multiple levels of tracking.
    /// we always need enough to handle billing, so even the "none" level still has some minimal tracking.
    /// This "accounting_key" is used in the relational database.
    /// anonymous users are also saved in the relational database so that the host can do their own cost accounting.
    fn accounting_key(&self, period_seconds: i64) -> RpcQueryKey {
        let response_timestamp = round_timestamp(self.response_timestamp, period_seconds);

        let rpc_secret_key_id = self.authorization.checks.rpc_secret_key_id;

        let (method, origin) = match self.authorization.checks.tracking_level {
            TrackingLevel::None => {
                // this RPC key requested no tracking. this is the default
                // do not store the method or the origin
                (None, None)
            }
            TrackingLevel::Aggregated => {
                // this RPC key requested tracking aggregated across all methods and origins
                // TODO: think about this more. do we want the origin or not? grouping free cost per site might be useful. i'd rather not collect things if we don't have a planned purpose though
                let method = None;
                let origin = None;

                (method, origin)
            }
            TrackingLevel::Detailed => {
                // detailed tracking keeps track of the method and origin
                // depending on the request, the origin might still be None
                let method = self.method.clone();
                let origin = self.authorization.origin.clone();

                (method, origin)
            }
        };

        // Depending on method, add some arithmetic around calculating credits_used
        // I think balance should not go here, this looks more like a key thingy
        RpcQueryKey {
            response_timestamp,
            archive_needed: self.archive_request,
            error_response: self.error_response,
            method,
            rpc_secret_key_id,
            origin,
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
            origin,
        }
    }

    /// rpc keys can opt into more detailed tracking
    fn opt_in_timeseries_key(&self) -> Option<RpcQueryKey> {
        // we don't store origin in the timeseries db. its only optionaly used for accounting
        let origin = None;

        // depending on tracking level, we either skip opt-in stats, track without method, or track with method
        let method = match self.authorization.checks.tracking_level {
            TrackingLevel::None => {
                // this RPC key requested no tracking. this is the default.
                return None;
            }
            TrackingLevel::Aggregated => {
                // this RPC key requested tracking aggregated across all methods
                None
            }
            TrackingLevel::Detailed => {
                // detailed tracking keeps track of the method
                self.method.clone()
            }
        };

        let key = RpcQueryKey {
            response_timestamp: self.response_timestamp,
            archive_needed: self.archive_request,
            error_response: self.error_response,
            method,
            rpc_secret_key_id: self.authorization.checks.rpc_secret_key_id,
            origin,
        };

        Some(key)
    }
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
        self.sum_credits_used += stat.credits_used;

        // Also record the latest balance for this user ..
        self.latest_balance = stat
            .authorization
            .checks
            .balance
            .unwrap_or(Decimal::from(0));
    }

    // TODO: take a db transaction instead so that we can batch?
    async fn save_db(
        self,
        chain_id: u64,
        db_conn: &DatabaseConnection,
        key: RpcQueryKey,
        rpc_secret_key_cache: Option<&RpcSecretKeyCache>,
    ) -> Web3ProxyResult<()> {
        if key.response_timestamp == 0 {
            return Err(Web3ProxyError::Anyhow(anyhow!(
                "no response_timestamp! This is a bug! {:?} {:?}",
                key,
                self
            )));
        }

        let period_datetime = Utc.timestamp_opt(key.response_timestamp, 0).unwrap();

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
            sum_credits_used: sea_orm::Set(self.sum_credits_used),
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
                            rpc_accounting_v2::Column::SumCreditsUsed,
                            Expr::col(rpc_accounting_v2::Column::SumCreditsUsed)
                                .add(self.sum_credits_used),
                        ),
                    ])
                    .to_owned(),
            )
            .exec(db_conn)
            .await?;

        // =============================== //
        // PREPARE FOR UPDATE USER BALANCE //
        // =============================== //
        let rpc_secret_key_id: u64 = match key.rpc_secret_key_id {
            Some(x) => x.into(),
            // Return early if the RPC key is not found, because then it is an anonymous user
            None => return Ok(()),
        };

        // =============================== //
        // GET ALL (STATIC) VARIABLES      //
        // =============================== //
        // Get the user with that RPC key. This is also the referee

        // Txn is not strictly necessary, but still good to keep things consistent across tables
        let txn = db_conn.begin().await?;

        let sender_rpc_entity = rpc_key::Entity::find()
            .filter(rpc_key::Column::Id.eq(rpc_secret_key_id))
            .one(&txn)
            .await?
            .ok_or(Web3ProxyError::BadRequest(
                "Could not find rpc key in db".to_string(),
            ))?;

        // I think one lock here is fine, because only one server has access to the "credits_applied_for_referee" entry
        let referral_objects = match referee::Entity::find()
            .filter(referee::Column::UserId.eq(sender_rpc_entity.user_id))
            .lock(LockType::Update)
            .find_also_related(referrer::Entity)
            .one(&txn)
            .await?
        {
            Some(x) => Some((
                x.0,
                x.1.context("Could not fine corresponding referrer code")?,
            )),
            None => None,
        };

        // ====================== //
        //     INITIATE DELTAS    //
        // ====================== //
        // Calculate Balance Only (No referrer)
        let mut sender_available_balance_delta = Decimal::from(-1) * self.sum_credits_used;
        let sender_used_balance_delta = self.sum_credits_used;
        let mut sender_bonus_applied;
        // Calculate Referrer Bonuses
        let mut referrer_balance_delta = Decimal::from(0);

        // ============================================================ //
        //  BASED ON REFERRERS, CALCULATE HOW MUCH SHOULD BE ATTRIBUTED //
        // ============================================================ //
        // If we don't lock the database as we do above on the referral_entry, we would have to do this operation on the database
        if let Some((referral_entity, referrer_code_entity)) = referral_objects {
            sender_bonus_applied = referral_entity.credits_applied_for_referee;

            // Calculate if we are above the usage threshold, and apply a bonus
            // Optimally we would read this from the balance, but if we do it like this, we only have to lock a single table (much safer w.r.t. deadlocks)
            if !referral_entity.credits_applied_for_referee
                && (referral_entity.credits_applied_for_referrer * (Decimal::from(10))
                    + self.sum_credits_used)
                    >= Decimal::from(100)
            {
                sender_available_balance_delta += Decimal::from(100);
                sender_bonus_applied = true;
            }

            // Calculate how much the referrer should get, limited to the last 12 months
            // Apply 10% of the used balance as a bonus if applicable
            let now = Utc::now();
            let valid_until = DateTime::<Utc>::from_utc(referral_entity.referral_start_date, Utc)
                + Months::new(12);

            if now <= valid_until {
                referrer_balance_delta += self.sum_credits_used / Decimal::new(10, 0);
            }

            // Do the referrer_entry updates
            if referrer_balance_delta > Decimal::from(0) {
                let referee_entry = referee::ActiveModel {
                    id: sea_orm::Unchanged(referral_entity.id),
                    referral_start_date: sea_orm::Unchanged(referral_entity.referral_start_date),
                    used_referral_code: sea_orm::Unchanged(referral_entity.used_referral_code),
                    user_id: sea_orm::Unchanged(referral_entity.user_id),

                    credits_applied_for_referee: sea_orm::Set(sender_bonus_applied),
                    credits_applied_for_referrer: sea_orm::Set(referrer_balance_delta),
                };
                referee::Entity::insert(referee_entry)
                    .on_conflict(
                        OnConflict::new()
                            .values([
                                (
                                    referee::Column::CreditsAppliedForReferee,
                                    // Make it a "Set"
                                    Expr::col(referee::Column::CreditsAppliedForReferee)
                                        .eq(sender_bonus_applied),
                                ),
                                (
                                    referee::Column::CreditsAppliedForReferrer,
                                    Expr::col(referee::Column::CreditsAppliedForReferrer)
                                        .add(referrer_balance_delta),
                                ),
                            ])
                            .to_owned(),
                    )
                    .exec(&txn)
                    .await?
                    .last_insert_id;
            }
        }

        // ================================= //
        //  UPDATE REFERRER & USER BALANCE   //
        // ================================= //
        let user_balance = balance::ActiveModel {
            id: sea_orm::NotSet,
            available_balance: sea_orm::Set(sender_available_balance_delta),
            used_balance: sea_orm::Set(sender_used_balance_delta),
            user_id: sea_orm::Set(sender_rpc_entity.user_id),
        };

        let _ = balance::Entity::insert(user_balance)
            .on_conflict(
                OnConflict::new()
                    .values([
                        (
                            balance::Column::AvailableBalance,
                            Expr::col(balance::Column::AvailableBalance)
                                .add(sender_available_balance_delta),
                        ),
                        (
                            balance::Column::UsedBalance,
                            Expr::col(balance::Column::UsedBalance).add(sender_used_balance_delta),
                        ),
                    ])
                    .to_owned(),
            )
            .exec(&txn)
            .await?
            .last_insert_id;

        if referrer_balance_delta > Decimal::from(0) {
            let user_balance = balance::ActiveModel {
                id: sea_orm::NotSet,
                available_balance: sea_orm::Set(referrer_balance_delta),
                used_balance: sea_orm::Set(Decimal::from(0)),
                user_id: sea_orm::Set(sender_rpc_entity.user_id),
            };

            let _ = balance::Entity::insert(user_balance)
                .on_conflict(
                    OnConflict::new()
                        .values([(
                            balance::Column::AvailableBalance,
                            Expr::col(balance::Column::AvailableBalance)
                                .add(referrer_balance_delta),
                        )])
                        .to_owned(),
                )
                .exec(&txn)
                .await?
                .last_insert_id;
        }

        // ================================ //
        // TODO: REFRESH USER ROLE IN CACHE //
        // ================================ //
        txn.commit()
            .await
            .context("Failed to update referral and balance updates")?;

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

        if let Some(method) = key.method {
            builder = builder.tag("method", method);
        }

        builder = builder
            .tag("archive_needed", key.archive_needed.to_string())
            .tag("error_response", key.error_response.to_string())
            .field("frontend_requests", self.frontend_requests as i64)
            .field("backend_requests", self.backend_requests as i64)
            .field("no_servers", self.no_servers as i64)
            .field("cache_misses", self.cache_misses as i64)
            .field("cache_hits", self.cache_hits as i64)
            .field("sum_request_bytes", self.sum_request_bytes as i64)
            .field("sum_response_millis", self.sum_response_millis as i64)
            .field("sum_response_bytes", self.sum_response_bytes as i64)
            // TODO: will this be enough of a range
            // I guess Decimal can be a f64
            // TODO: This should prob be a float, i should change the query if we want float-precision for this (which would be important...)
            .field(
                "sum_credits_used",
                self.sum_credits_used
                    .to_f64()
                    .expect("number is really (too) large"),
            )
            .field(
                "balance",
                self.latest_balance
                    .to_f64()
                    .expect("number is really (too) large"),
            );

        // .round() as i64

        builder = builder.timestamp(key.response_timestamp);

        let point = builder.build()?;

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
        let mut response_millis = metadata.response_millis.load(atomic::Ordering::Acquire);

        let response_timestamp = match metadata.response_timestamp.load(atomic::Ordering::Acquire) {
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

        let method = metadata.method.take();

        let credits_used = Self::compute_cost(
            request_bytes,
            response_bytes,
            backend_rpcs_used.is_empty(),
            method.as_deref(),
        );

        let x = Self {
            authorization,
            archive_request,
            method,
            backend_rpcs_used,
            request_bytes,
            error_response,
            response_bytes,
            response_millis,
            response_timestamp,
            credits_used,
        };

        Ok(x)
    }
}

impl RpcQueryStats {
    /// Compute cost per request
    /// All methods cost the same
    /// The number of bytes are based on input, and output bytes
    pub fn compute_cost(
        request_bytes: u64,
        response_bytes: u64,
        cache_hit: bool,
        method: Option<&str>,
    ) -> Decimal {
        // for now, always return 0 for cost
        0.into()

        /*
        // some methods should be free. there might be cases where method isn't set (though they should be uncommon)
        // TODO: get this list from config (and add more to it)
        if let Some(method) = method.as_ref() {
            if ["eth_chainId"].contains(method) {
                return 0.into();
            }
        }

        // TODO: get cost_minimum, cost_free_bytes, cost_per_byte, cache_hit_divisor from config. each chain will be different
        // pays at least $0.000018 / credits per request
        let cost_minimum = Decimal::new(18, 6);

        // 1kb is included on each call
        let cost_free_bytes = 1024;

        // after that, we add cost per bytes, $0.000000006 / credits per byte
        // amazon charges $.09/GB outbound
        // but we also have to cover our RAM and expensive nics on the servers (haproxy/web3-proxy/blockchains)
        let cost_per_byte = Decimal::new(6, 9);

        let total_bytes = request_bytes + response_bytes;

        let total_chargable_bytes = Decimal::from(total_bytes.saturating_sub(cost_free_bytes));

        let mut cost = cost_minimum + cost_per_byte * total_chargable_bytes;

        // cache hits get a 50% discount
        if cache_hit {
            cost /= Decimal::from(2)
        }

        cost
        */
    }
}
