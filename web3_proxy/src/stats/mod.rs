//! Store "stats" in a database for billing and a different database for graphing
//! TODO: move some of these structs/functions into their own file?
mod stat_buffer;

pub mod db_queries;
pub mod influxdb_queries;

use self::stat_buffer::BufferedRpcQueryStats;
use crate::caches::{RpcSecretKeyCache, UserBalanceCache};
use crate::compute_units::ComputeUnit;
use crate::errors::{Web3ProxyError, Web3ProxyResult};
use crate::frontend::authorization::{Authorization, AuthorizationType, Web3Request};
use crate::rpcs::one::Web3Rpc;
use anyhow::{anyhow, Context};
use chrono::{DateTime, Months, TimeZone, Utc};
use derive_more::{AddAssign, From};
use entities::{referee, referrer, rpc_accounting_v2};
use influxdb2::models::DataPoint;
use migration::sea_orm::prelude::Decimal;
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, IntoActiveModel,
    QueryFilter, QuerySelect, TransactionTrait,
};
use migration::{Expr, LockType, OnConflict};
use num_traits::ToPrimitive;
use parking_lot::Mutex;
use std::borrow::Cow;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tracing::{error, instrument, trace, warn};

pub use stat_buffer::{SpawnedStatBuffer, StatBuffer};

#[derive(Debug, PartialEq, Eq)]
pub enum StatType {
    Aggregated,
    Detailed,
}

pub type BackendRequests = Mutex<Vec<Arc<Web3Rpc>>>;

#[derive(AddAssign, Copy, Clone, Debug, Default)]
pub struct FlushedStats {
    /// the number of rows saved to the relational database.
    /// rows can contain multiple requests
    pub relational: usize,
    pub relational_frontend_requests: u64,
    pub relational_internal_requests: u64,
    /// the number of data points saved to the timeseries database.
    /// data points can contain multiple requests
    pub timeseries: usize,
    /// the number of global frontend requests saved to the time series database
    pub timeseries_frontend_requests: u64,
    pub timeseries_internal_requests: u64,
}

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
    pub authorization_type: AuthorizationType,
    /// unix epoch time in seconds.
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
    /// 0 if the public url was used.
    rpc_secret_key_id: u64,
    /// 0 if the public url was used.
    /// TODO: u64::MAX if the internal? or have a migration make a user for us? or keep 0 and we track that another way?
    rpc_key_user_id: u64,
}

impl RpcQueryKey {
    pub fn is_registered(&self) -> bool {
        self.rpc_key_user_id != 0
    }
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

        // it is very important that for anonymous users, rpc_secret_key_id is 0 and not NULL in the database
        // for unique indexes, sql sees each NULL as a unique value!
        // but we want them grouped!
        let rpc_secret_key_id = self
            .authorization
            .checks
            .rpc_secret_key_id
            .map(u64::from)
            .unwrap_or_default();

        let method = self.method.clone();

        // user_error_response is always set to false because we don't bother tracking this in the database
        let user_error_response = false;

        // Depending on method, add some arithmetic around calculating credits_used
        // I think balance should not go here, this looks more like a key thingy
        RpcQueryKey {
            authorization_type: self.authorization.authorization_type,
            response_timestamp,
            archive_needed: self.archive_request,
            error_response: self.error_response,
            method,
            rpc_secret_key_id,
            rpc_key_user_id: self.authorization.checks.user_id,
            user_error_response,
        }
    }

    /// all rpc keys are aggregated in the global stats
    /// TODO: should we store "anon" or "registered" as a key just to be able to split graphs?
    fn global_timeseries_key(&self) -> RpcQueryKey {
        // we include the method because that can be helpful for predicting load
        let method = self.method.clone();

        // everyone gets grouped together
        let rpc_secret_key_id = 0;
        let rpc_key_user_id = 0;

        RpcQueryKey {
            authorization_type: self.authorization.authorization_type,
            response_timestamp: self.response_timestamp,
            archive_needed: self.archive_request,
            error_response: self.error_response,
            method,
            rpc_secret_key_id,
            rpc_key_user_id,
            user_error_response: self.user_error_response,
        }
    }

    /// stats for a single key
    fn owned_timeseries_key(&self, active_premium: bool) -> Option<RpcQueryKey> {
        if !active_premium {
            return None;
        }

        let rpc_secret_key_id = self
            .authorization
            .checks
            .rpc_secret_key_id
            .map(u64::from)
            .unwrap_or_default();

        let method = self.method.clone();

        let key = RpcQueryKey {
            authorization_type: self.authorization.authorization_type,
            response_timestamp: self.response_timestamp,
            archive_needed: self.archive_request,
            error_response: self.error_response,
            method,
            rpc_secret_key_id,
            rpc_key_user_id: self.authorization.checks.user_id,
            user_error_response: self.user_error_response,
        };

        Some(key)
    }
}

/// A stat that we aggregate and then store in a database.
/// For now there is just one, but I think there might be others later
#[derive(Debug, From)]
pub enum AppStat {
    RpcQuery(Web3Request),
}

// TODO: move to stat_buffer.rs?
impl BufferedRpcQueryStats {
    #[instrument(level = "trace")]
    async fn add(&mut self, stat: RpcQueryStats, approximate_balance_remaining: Option<Decimal>) {
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

        if stat.authorization.checks.paid_credits_used {
            self.paid_credits_used += stat.compute_unit_cost;
        }

        if approximate_balance_remaining.is_some() {
            // notice that we overwrite. we intentionally do not increment!
            self.approximate_balance_remaining = approximate_balance_remaining;
        }

        trace!("added");
    }

    async fn _save_db_stats(
        &self,
        chain_id: u64,
        db_conn: &DatabaseConnection,
        key: &RpcQueryKey,
    ) -> Web3ProxyResult<()> {
        let period_datetime = Utc.timestamp_opt(key.response_timestamp, 0).unwrap();

        // =============================== //
        //       UPDATE STATISTICS         //
        // =============================== //
        let accounting_entry = rpc_accounting_v2::ActiveModel {
            id: sea_orm::NotSet,
            // eventually rpc_key_id will be `NOT NULL`, but we have old data in the db to deal with
            rpc_key_id: sea_orm::Set(Some(key.rpc_secret_key_id)),
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
            sum_credits_used: sea_orm::Set(self.paid_credits_used),
            sum_incl_free_credits_used: sea_orm::Set(self.sum_credits_used),
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
                            rpc_accounting_v2::Column::SumInclFreeCreditsUsed,
                            Expr::col(rpc_accounting_v2::Column::SumInclFreeCreditsUsed)
                                .add(self.sum_credits_used),
                        ),
                        (
                            rpc_accounting_v2::Column::SumCreditsUsed,
                            Expr::col(rpc_accounting_v2::Column::SumCreditsUsed)
                                .add(self.paid_credits_used),
                        ),
                    ])
                    .to_owned(),
            )
            .exec(db_conn)
            .await?;

        Ok(())
    }

    // TODO: take a db transaction instead so that we can batch?
    async fn save_db(
        self,
        chain_id: u64,
        db_conn: &DatabaseConnection,
        key: RpcQueryKey,
        user_balance_cache: &UserBalanceCache,
        rpc_secret_key_cache: &RpcSecretKeyCache,
    ) -> Web3ProxyResult<()> {
        // Sanity check, if we need to save stats
        if key.response_timestamp == 0 {
            return Err(Web3ProxyError::Anyhow(anyhow!(
                "no response_timestamp! This is a bug! {:?} {:?}",
                key,
                self
            )));
        }

        // TODO: rename to owner_id?
        let sender_user_id = key.rpc_key_user_id;

        // save the statistics to the database:
        self._save_db_stats(chain_id, db_conn, &key).await?;

        // Apply all the referral logic; let's keep it simple and flat for now
        if self.paid_credits_used > 0.into() {
            let mut invalidate_caches = false;

            // Start a transaction
            let txn = db_conn.begin().await?;

            // Calculate if we are above the usage threshold, and apply a bonus
            // Optimally we would read this from the balance, but if we do it like this, we only have to lock a single table (much safer w.r.t. deadlocks)
            // referral_entity.credits_applied_for_referrer * (Decimal::from(10) checks (atomically using this table only), whether the user has brought in >$100 to the referer
            // In this case, the sender receives $100 as a bonus / gift
            // Apply a 10$ bonus onto the user, if the user has spent 100$
            // TODO: i think we do want a LockType::Update on this
            match referee::Entity::find()
                .lock(LockType::Update)
                .filter(referee::Column::UserId.eq(sender_user_id))
                .find_also_related(referrer::Entity)
                .one(&txn)
                .await?
            {
                Some((referral_entity, Some(referrer))) => {
                    // Get the balance for the referrer, see if they're premium or not
                    let referrer_balance = user_balance_cache
                        .get_or_insert(db_conn, referrer.user_id)
                        .await?;

                    // Just to keep locking simple, read and clone. if the value is slightly delayed, that is okay
                    let referrer_balance = referrer_balance.read().await.clone();

                    // Apply the bonuses only if they have the necessary premium statuses
                    if referrer_balance.was_ever_premium() {
                        // spend $100
                        let bonus_for_user_threshold = Decimal::from(100);
                        // get $10
                        let bonus_for_user = Decimal::from(10);

                        let referral_start_date = referral_entity.referral_start_date;

                        let mut referral_entity = referral_entity.into_active_model();

                        // Provide one-time bonus to user, if more than 100$ was spent,
                        // and if the one-time bonus was not already provided
                        // TODO: make sure that if we change the bonus from 10%, we also change this multiplication of 10!
                        if referral_entity
                            .one_time_bonus_applied_for_referee
                            .as_ref()
                            .is_zero()
                            && (referral_entity.credits_applied_for_referrer.as_ref()
                                * Decimal::from(10)
                                + self.sum_credits_used)
                                >= bonus_for_user_threshold
                        {
                            trace!("Adding sender bonus balance");

                            referral_entity.one_time_bonus_applied_for_referee =
                                sea_orm::Set(bonus_for_user);

                            // writing here with `+= 10` has a race unless we lock outside of the mysql query (and thats just too slow)
                            // so instead we just invalidate the cache (after writing to mysql)
                            invalidate_caches = true;
                        }

                        let now = Utc::now();
                        let valid_until =
                            DateTime::<Utc>::from_naive_utc_and_offset(referral_start_date, Utc)
                                + Months::new(12);

                        // If the referrer ever had premium, provide credits to them
                        // Also only works if the referrer referred the person less than 1 year ago
                        // TODO: Perhaps let's not worry about the referral cache here, to avoid deadlocks (hence only reading)

                        if now <= valid_until {
                            // TODO: make this configurable (and change all the other hard coded places for 10%)
                            let referrer_bonus = self.paid_credits_used / Decimal::from(10);

                            // there is a LockType::Update on this that should keep any raises incrementing this
                            referral_entity.credits_applied_for_referrer = sea_orm::Set(
                                referral_entity.credits_applied_for_referrer.as_ref()
                                    + referrer_bonus,
                            );
                            // No need to invalidate the referrer every single time;
                            // this is no major change and can wait for a bit
                            // Let's not worry about the referrer balance bcs possibility of deadlock
                            // referrer_balance.total_deposits += referrer_bonus;
                        }

                        // The resulting field will not be read again, so I will not try to turn the ActiveModel into a Model one
                        referral_entity.save(&txn).await?;
                    }
                }
                Some((referee, None)) => {
                    error!(
                        ?referee,
                        "No referrer code found for this referrer, this should never happen!",
                    );
                }
                _ => {}
            };

            // Finally, commit the transaction in the database
            txn.commit()
                .await
                .context("Failed to update referral and balance updates")?;

            if invalidate_caches {
                if let Err(err) = user_balance_cache
                    .invalidate(&sender_user_id, db_conn, rpc_secret_key_cache)
                    .await
                {
                    warn!(?err, "unable to invalidate caches");
                };
            }
        }

        Ok(())
    }

    async fn build_timeseries_point(
        self,
        measurement: &str,
        chain_id: u64,
        key: RpcQueryKey,
        uniq: i64,
    ) -> anyhow::Result<DataPoint> {
        let mut builder = DataPoint::builder(measurement)
            .tag("archive_needed", key.archive_needed.to_string())
            .tag("chain_id", chain_id.to_string())
            .tag("error_response", key.error_response.to_string())
            .tag("method", key.method)
            .tag("user_error_response", key.user_error_response.to_string())
            .field("backend_requests", self.backend_requests as i64)
            .field("cache_hits", self.cache_hits as i64)
            .field("cache_misses", self.cache_misses as i64)
            .field("frontend_requests", self.frontend_requests as i64)
            .field("no_servers", self.no_servers as i64)
            .field("sum_request_bytes", self.sum_request_bytes as i64)
            .field("sum_response_bytes", self.sum_response_bytes as i64)
            .field("sum_response_millis", self.sum_response_millis as i64)
            .field(
                "sum_credits_used",
                self.paid_credits_used
                    .to_f64()
                    .context("sum_credits_used is really (too) large")?,
            )
            .field(
                "sum_incl_free_credits_used",
                self.sum_credits_used
                    .to_f64()
                    .context("sum_credits_used is really (too) large")?,
            );

        if let Some(balance) = self.approximate_balance_remaining {
            builder = builder.field(
                "balance",
                balance.to_f64().context("balance is really (too) large")?,
            )
        }

        // TODO: set the rpc_secret_key_id tag to 0 when anon? will that make other queries easier?
        if key.rpc_secret_key_id != 0 {
            builder = builder.tag("rpc_secret_key_id", key.rpc_secret_key_id.to_string());
        }

        // [add "uniq" to the timestamp](https://docs.influxdata.com/influxdb/v2.0/write-data/best-practices/duplicate-points/#increment-the-timestamp)
        // i64 timestamps get us to Friday, April 11, 2262
        assert!(uniq < 1_000_000_000, "uniq is way too big");
        let timestamp_ns: i64 = key.response_timestamp * 1_000_000_000 + uniq;
        builder = builder.timestamp(timestamp_ns);

        let point = builder.build()?;

        trace!("Datapoint saving to Influx is {:?}", point);

        Ok(point)
    }
}

/// this is **intentionally** not a TryFrom<Arc<RequestMetadata>>
/// We want this to run when there is **one and only one** copy of this RequestMetadata left
/// There are often multiple copies if a request is being sent to multiple servers in parallel
impl RpcQueryStats {
    fn try_from_metadata(metadata: Web3Request) -> Web3ProxyResult<Self> {
        // TODO: do this without a clone
        let authorization = metadata.authorization.clone();

        let archive_request = metadata.archive_request.load(Ordering::Relaxed);

        // TODO: do this without cloning. we can take their vec
        let backend_rpcs_used = metadata.backend_rpcs_used();

        let request_bytes = metadata.request.num_bytes() as u64;
        let response_bytes = metadata.response_bytes.load(Ordering::Relaxed);

        let mut error_response = metadata.error_response.load(Ordering::Relaxed);
        let mut response_millis = metadata.response_millis.load(Ordering::Relaxed);

        let user_error_response = metadata.user_error_response.load(Ordering::Relaxed);

        let response_timestamp = match metadata.response_timestamp.load(Ordering::Relaxed) {
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

        let cu = ComputeUnit::new(metadata.request.method(), metadata.chain_id, response_bytes);

        let cache_hit = backend_rpcs_used.is_empty();

        let compute_unit_cost = cu.cost(
            archive_request,
            cache_hit,
            error_response,
            &metadata.usd_per_cu,
        );

        let method = metadata.request.method().to_string().into();

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

#[cfg(test)]
mod tests {
    use crate::test_utils::TestInflux;
    use crate::{caches::UserBalanceCache, stats::StatBuffer};
    use moka::future::Cache;
    use tokio::sync::{broadcast, mpsc};

    #[cfg_attr(not(feature = "tests-needing-docker"), ignore)]
    #[test_log::test(tokio::test)]
    async fn test_two_buffers() {
        let i = TestInflux::spawn().await;

        let billing_period_seconds = 86400 * 7;
        let chain_id = 999_001_999;
        let db_save_interval_seconds = 60;
        let influxdb_bucket = Some(i.bucket.clone());
        let influxdb_client = Some(i.client.clone());
        let rpc_secret_key_cache = Cache::builder().build();
        let tsdb_save_interval_seconds = 30;
        let user_balance_cache: UserBalanceCache = Cache::builder().build().into();

        let (shutdown_sender, shutdown_receiver_1) = broadcast::channel(1);
        let shutdown_receiver_2 = shutdown_sender.subscribe();

        let (flush_sender_1, flush_receiver_1) = mpsc::channel(1);
        let (flush_sender_2, flush_receiver_2) = mpsc::channel(1);

        let buffer_1 = StatBuffer::try_spawn(
            billing_period_seconds,
            chain_id,
            db_save_interval_seconds,
            influxdb_bucket.clone(),
            influxdb_client.clone(),
            rpc_secret_key_cache.clone(),
            user_balance_cache.clone(),
            shutdown_receiver_1,
            tsdb_save_interval_seconds,
            flush_sender_1,
            flush_receiver_1,
            1,
        )
        .unwrap()
        .unwrap();

        let buffer_2 = StatBuffer::try_spawn(
            billing_period_seconds,
            chain_id,
            db_save_interval_seconds,
            influxdb_bucket,
            influxdb_client,
            rpc_secret_key_cache,
            user_balance_cache,
            shutdown_receiver_2,
            tsdb_save_interval_seconds,
            flush_sender_2,
            flush_receiver_2,
            2,
        )
        .unwrap()
        .unwrap();

        // TODO: send things to the buffers

        shutdown_sender.send(()).unwrap();

        buffer_1.background_handle.await.unwrap().unwrap();
        buffer_2.background_handle.await.unwrap().unwrap();
    }
}
