//! Store "stats" in a database for billing and a different database for graphing
//! TODO: move some of these structs/functions into their own file?
pub mod db_queries;
pub mod influxdb_queries;
mod stat_buffer;

pub use stat_buffer::{SpawnedStatBuffer, StatBuffer};
use std::borrow::BorrowMut;
use std::cmp;

use crate::app::{RpcSecretKeyCache, UserBalanceCache};
use crate::errors::{Web3ProxyError, Web3ProxyResult};
use crate::frontend::authorization::{Authorization, RequestMetadata, RpcSecretKey};
use crate::rpcs::one::Web3Rpc;
use anyhow::{anyhow, Context};
use axum::headers::Origin;
use chrono::{DateTime, Months, TimeZone, Utc};
use derive_more::From;
use entities::sea_orm_active_enums::TrackingLevel;
use entities::{balance, referee, referrer, rpc_accounting_v2, rpc_key, user, user_tier};
use influxdb2::models::DataPoint;
use log::{error, info, trace, warn};
use migration::sea_orm::prelude::Decimal;
use migration::sea_orm::QuerySelect;
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, IntoActiveModel,
    QueryFilter, TransactionTrait,
};
use migration::{Expr, LockType, OnConflict, Order};
use num_traits::{clamp, clamp_min, ToPrimitive};
use parking_lot::Mutex;
use std::cmp::max;
use std::num::NonZeroU64;
use std::sync::atomic::{self, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

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
    /// Last credits used
    pub latest_balance: Arc<RwLock<Decimal>>,
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
        // Also subtract the used balance from the cache so we
        // TODO: We are already using the cache. We could also inject the cache into save_tsdb
        self.latest_balance = stat.latest_balance;
    }

    /// Check a user's balance and possibly downgrade him in the cache
    async fn downgrade_user(self) {}

    // TODO: take a db transaction instead so that we can batch?
    async fn save_db(
        self,
        chain_id: u64,
        db_conn: &DatabaseConnection,
        key: RpcQueryKey,
        rpc_secret_key_cache: RpcSecretKeyCache,
        user_balance_cache: UserBalanceCache,
    ) -> Web3ProxyResult<()> {
        if key.response_timestamp == 0 {
            return Err(Web3ProxyError::Anyhow(anyhow!(
                "no response_timestamp! This is a bug! {:?} {:?}",
                key,
                self
            )));
        }

        let period_datetime = Utc.timestamp_opt(key.response_timestamp, 0).unwrap();

        // TODO: Could add last balance here (can take the element from the cache, and RpcQueryKey::AuthorizationCheck)

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
        //       GET ALL VARIABLES         //
        // =============================== //
        // Get all the variables that we might be working with
        let txn = db_conn.begin().await?;

        // (1) Get the user with that RPC key. This is also the referee
        let sender_rpc_entity = rpc_key::Entity::find()
            .filter(rpc_key::Column::Id.eq(rpc_secret_key_id))
            .lock(LockType::Update)
            .one(&txn)
            .await?
            .ok_or(Web3ProxyError::BadRequest(
                "Could not find rpc key in db".into(),
            ))?;

        let sender_balance = balance::Entity::find()
            .filter(balance::Column::UserId.eq(sender_rpc_entity.user_id))
            .lock(LockType::Update)
            .one(&txn)
            .await?
            .ok_or(Web3ProxyError::BadRequest(
                format!("This user id has no balance entry! {:?}", sender_rpc_entity).into(),
            ))?;

        // TODO: Also make sure that the referrer is premium, otherwise do not assign credits
        // This will be optional
        let referral_objects = match referee::Entity::find()
            .filter(referee::Column::UserId.eq(sender_rpc_entity.user_id))
            .lock(LockType::Update)
            .one(&txn)
            .await?
        {
            Some(referee_entity) => {
                // In this case, also fetch the referrer
                match referrer::Entity::find()
                    .filter(referrer::Column::Id.eq(referee_entity.used_referral_code))
                    .lock(LockType::Update)
                    .one(&txn)
                    .await?
                {
                    Some(referrer_connection) => {
                        // Get the referring user and their balance
                        let referrer_user_entity = user::Entity::find()
                            .filter(user::Column::Id.eq(referrer_connection.user_id))
                            .lock(LockType::Update)
                            .one(&txn)
                            .await?
                            .ok_or(Web3ProxyError::BadRequest(
                                "Could not find rpc key in db".into(),
                            ))?;
                        // And their bala
                        let referrer_balance_entity = balance::Entity::find()
                            .filter(balance::Column::UserId.eq(referrer_connection.user_id))
                            .lock(LockType::Update)
                            .one(&txn)
                            .await?
                            .ok_or(Web3ProxyError::BadRequest(
                                format!(
                                    "This user id has no balance entry! {:?}",
                                    sender_rpc_entity
                                )
                                .into(),
                            ))?;
                        Some((
                            referee_entity,
                            referrer_user_entity,
                            referrer_balance_entity,
                        ))
                    }
                    None => None,
                }
            }
            None => None,
        };

        // =============================== //
        //    UPDATE CALLER BALANCE        //
        // =============================== //
        // Update is regardless of referrals

        // I think I can update the balance naively now basically
        let mut active_sender_balance = sender_balance.clone().into_active_model();
        active_sender_balance.available_balance = sea_orm::Set(cmp::max(
            Decimal::from(0),
            sender_balance.available_balance - self.sum_credits_used,
        ));
        active_sender_balance.used_balance =
            sea_orm::Set(sender_balance.available_balance + self.sum_credits_used);

        // ================================= //
        // UPDATE REFERRER & REFEREE BALANCE //
        // ================================= //
        // Only branch into this if the referrer logic applies, i.e. if a referral logic applies
        if let Some((referee_entity, referrer_user_entity, referrer_balance)) = referral_objects {
            // update the referrer balance
            // Turn everything into active models that we can modify
            let referee_balance = active_sender_balance;
            let mut active_referee_balance = referee_balance.clone().into_active_model();
            let mut active_referee_entity = referee_entity.clone().into_active_model();
            let mut active_referrer_balance = referrer_balance.clone().into_active_model();

            // If the credits have not yet been applied to the referee, apply 10M credits / $100.00 USD worth of credits.
            // TODO: Hardcode this parameter also in config, so it's easier to tune
            if !referee_entity.credits_applied_for_referee
                && (referee_balance.used_balance.unwrap() + self.sum_credits_used)
                    >= Decimal::from(100)
            {
                active_referee_balance.available_balance =
                    sea_orm::Set(referee_balance.available_balance.unwrap() + Decimal::from(100));
                active_referee_entity.credits_applied_for_referee = sea_orm::Set(true);
                active_referee_balance.save(&txn).await?;
            }

            // Also apply some (10%) credits to the referrer if the referral is not too old
            let now = Utc::now();
            let valid_until = DateTime::<Utc>::from_utc(referee_entity.referral_start_date, Utc)
                .checked_add_months(Months::new(12))
                .unwrap();

            if now <= valid_until {
                active_referrer_balance.available_balance = sea_orm::Set(
                    referrer_balance.available_balance
                        + self.sum_credits_used / Decimal::new(10, 0),
                );
                // Also record how much the current referrer has "provided" / "gifted" away
                active_referee_entity.credits_applied_for_referrer = sea_orm::Set(
                    referee_entity.credits_applied_for_referrer + self.sum_credits_used,
                );
                active_referrer_balance.save(&txn).await?;
            }
            // Do this if anything has changed, otherwise it's redundant
            // We start the transaction anyways though, so that's fine
            active_referee_entity.save(&txn).await?;
        }

        // ============================================================= //
        //  INVALIDATE USER CACHE FOR CALCULATION IF BALANCE IS TOO LOW  //
        // ============================================================= //
        // Gotta update the balance in authorization checks ...

        // Invalidate cache if user is below 10$ credits (premium downgrade condition)
        // Reduce credits if there was no issue
        // This is not atomic, so this may be an issue because it's not sequentially consistent across threads
        // It is a good-enough approximation though, and if the TTL for the balance cache is high enough, this should be ok
        // TODO: Ask about feedback here, prob cache with no modification, and some read-write may be more accurate ...
        // TODO: We already cal balance above as well, but here we primarily rely on the cache. It's probably fine ...
        let latest_balance = match NonZeroU64::try_from(sender_rpc_entity.user_id) {
            Err(_) => Arc::new(RwLock::new(Decimal::default())),
            Ok(x) => {
                user_balance_cache
                    .get_or_insert_async(&x, async move {
                        Arc::new(RwLock::new(sender_balance.available_balance))
                    })
                    .await
            }
        };
        // Lock it, subtract and max it
        let mut latest_balance = latest_balance.write().await;
        // Double check that this copies correctly (the underlying value, not the reference)
        let balance_before = (*latest_balance).clone();
        // Now modify the balance
        *latest_balance = *latest_balance - self.sum_credits_used;
        if *latest_balance < Decimal::from(0) {
            *latest_balance = Decimal::from(0);
        }

        // Also check if the referrer is premium (thought above 10$ will always be treated as premium at least)
        // Should only refresh cache if the premium threshold is crossed
        if balance_before >= self.sum_credits_used && *latest_balance < self.sum_credits_used {
            let rpc_keys = rpc_key::Entity::find()
                .filter(rpc_key::Column::UserId.eq(sender_rpc_entity.user_id))
                .all(&txn)
                .await?;

            for rpc_key_entity in rpc_keys {
                // TODO: Not sure which one was inserted, just delete both ...
                rpc_secret_key_cache.remove(&rpc_key_entity.secret_key.into());
            }
        }

        txn.commit().await?;

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

        // Read the latest balance ...
        let balance;
        {
            balance = *(self.latest_balance.read().await);
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
                    .context("number is really (too) large")?,
            )
            .field(
                "balance",
                balance.to_f64().context("number is really (too) large")?,
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
            // To we need to clone it here ... (?)
            latest_balance: metadata.latest_balance.clone(),
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
