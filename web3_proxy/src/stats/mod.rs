//! Store "stats" in a database for billing and a different database for graphing
//! TODO: move some of these structs/functions into their own file?
pub mod db_queries;
pub mod influxdb_queries;
use crate::frontend::authorization::{Authorization, RequestMetadata};
use anyhow::Context;
use axum::headers::Origin;
use chrono::{DateTime, Months, TimeZone, Utc};
use derive_more::From;
use entities::sea_orm_active_enums::TrackingLevel;
use entities::{balance, referee, referrer, rpc_accounting_v2, rpc_key, user, user_tier};
use futures::stream;
use hashbrown::HashMap;
use influxdb2::api::write::TimestampPrecision;
use influxdb2::models::DataPoint;
use log::{error, info, warn};
use migration::sea_orm::prelude::Decimal;
use migration::sea_orm::ActiveModelTrait;
use migration::sea_orm::ColumnTrait;
use migration::sea_orm::IntoActiveModel;
use migration::sea_orm::{self, DatabaseConnection, EntityTrait, QueryFilter};
use migration::{Expr, OnConflict};
use num_traits::ToPrimitive;
use std::cmp::max;
use std::num::NonZeroU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::interval;

#[derive(Debug, PartialEq, Eq)]
pub enum StatType {
    Aggregated,
    Detailed,
}

// Pub is needed for migration ... I could also write a second constructor for this if needed
/// TODO: better name?
#[derive(Clone, Debug)]
pub struct RpcQueryStats {
    pub authorization: Arc<Authorization>,
    pub method: Option<String>,
    pub archive_request: bool,
    pub error_response: bool,
    pub request_bytes: u64,
    /// if backend_requests is 0, there was a cache_hit
    // pub frontend_request: u64,
    pub backend_requests: u64,
    pub response_bytes: u64,
    pub response_millis: u64,
    pub response_timestamp: i64,
    /// Credits used signifies how how much money was used up
    pub credits_used: Decimal,
}

#[derive(Clone, From, Hash, PartialEq, Eq)]
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

    /// all queries are aggregated
    /// TODO: should we store "anon" or "registered" as a key just to be able to split graphs?
    fn global_timeseries_key(&self) -> RpcQueryKey {
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

    fn opt_in_timeseries_key(&self) -> RpcQueryKey {
        // we don't store origin in the timeseries db. its only optionaly used for accounting
        let origin = None;

        let (method, rpc_secret_key_id) = match self.authorization.checks.tracking_level {
            TrackingLevel::None => {
                // this RPC key requested no tracking. this is the default.
                // we still want graphs though, so we just use None as the rpc_secret_key_id
                (self.method.clone(), None)
            }
            TrackingLevel::Aggregated => {
                // this RPC key requested tracking aggregated across all methods
                (None, self.authorization.checks.rpc_secret_key_id)
            }
            TrackingLevel::Detailed => {
                // detailed tracking keeps track of the method
                (
                    self.method.clone(),
                    self.authorization.checks.rpc_secret_key_id,
                )
            }
        };

        RpcQueryKey {
            response_timestamp: self.response_timestamp,
            archive_needed: self.archive_request,
            error_response: self.error_response,
            method,
            rpc_secret_key_id,
            origin,
        }
    }
}

#[derive(Default)]
pub struct BufferedRpcQueryStats {
    pub frontend_requests: u64,
    pub backend_requests: u64,
    pub backend_retries: u64,
    pub no_servers: u64,
    pub cache_misses: u64,
    pub cache_hits: u64,
    pub sum_request_bytes: u64,
    pub sum_response_bytes: u64,
    pub sum_response_millis: u64,
    pub sum_credits_used: Decimal,
    /// Balance tells us the user's balance at this point in time
    pub latest_balance: Decimal,
}

/// A stat that we aggregate and then store in a database.
/// For now there is just one, but I think there might be others later
#[derive(Debug, From)]
pub enum AppStat {
    RpcQuery(RpcQueryStats),
}

#[derive(From)]
pub struct SpawnedStatBuffer {
    pub stat_sender: flume::Sender<AppStat>,
    /// these handles are important and must be allowed to finish
    pub background_handle: JoinHandle<anyhow::Result<()>>,
}

pub struct StatBuffer {
    chain_id: u64,
    db_conn: Option<DatabaseConnection>,
    influxdb_client: Option<influxdb2::Client>,
    tsdb_save_interval_seconds: u32,
    db_save_interval_seconds: u32,
    billing_period_seconds: i64,
}

impl BufferedRpcQueryStats {
    fn add(&mut self, stat: RpcQueryStats) {
        // a stat always come from just 1 frontend request
        self.frontend_requests += 1;

        if stat.backend_requests == 0 {
            // no backend request. cache hit!
            self.cache_hits += 1;
        } else {
            // backend requests! cache miss!
            self.cache_misses += 1;

            // a single frontend request might have multiple backend requests
            self.backend_requests += stat.backend_requests;
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
    ) -> anyhow::Result<()> {
        let period_datetime = Utc.timestamp_opt(key.response_timestamp, 0).unwrap();

        // this is a lot of variables
        let accounting_entry = rpc_accounting_v2::ActiveModel {
            id: sea_orm::NotSet,
            rpc_key_id: sea_orm::Set(key.rpc_secret_key_id.map(Into::into).unwrap_or_default()),
            origin: sea_orm::Set(key.origin.map(|x| x.to_string()).unwrap_or_default()),
            chain_id: sea_orm::Set(chain_id),
            period_datetime: sea_orm::Set(period_datetime),
            method: sea_orm::Set(key.method.unwrap_or_default()),
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

        // TODO: Refactor this function a bit more just so it looks and feels nicer
        // TODO: Figure out how to go around unmatching, it shouldn't return an error, but this is disgusting

        // All the referral & balance arithmetic takes place here
        let rpc_secret_key_id: u64 = match key.rpc_secret_key_id {
            Some(x) => x.into(),
            // Return early if the RPC key is not found, because then it is an anonymous user
            None => return Ok(()),
        };

        // (1) Get the user with that RPC key. This is the referee
        let sender_rpc_key = rpc_key::Entity::find()
            .filter(rpc_key::Column::Id.eq(rpc_secret_key_id))
            .one(db_conn)
            .await?;

        // Technicall there should always be a user ... still let's return "Ok(())" for now
        let sender_user_id: u64 = match sender_rpc_key {
            Some(x) => x.user_id.into(),
            // Return early if the User is not found, because then it is an anonymous user
            // Let's also issue a warning because obviously the RPC key should correspond to a user
            None => {
                warn!(
                    "No user was found for the following rpc key: {:?}",
                    rpc_secret_key_id
                );
                return Ok(());
            }
        };

        // (1) Do some general bookkeeping on the user
        let sender_balance = match balance::Entity::find()
            .filter(balance::Column::UserId.eq(sender_user_id))
            .one(db_conn)
            .await?
        {
            Some(x) => x,
            None => {
                warn!("This user id has no balance entry! {:?}", sender_user_id);
                return Ok(());
            }
        };

        let mut active_sender_balance = sender_balance.clone().into_active_model();

        // Still subtract from the user in any case,
        // Modify the balance of the sender completely (in mysql, next to the stats)
        // In any case, add this to "spent"
        active_sender_balance.used_balance =
            sea_orm::Set(sender_balance.used_balance + Decimal::from(self.sum_credits_used));

        // Also update the available balance
        let new_available_balance = max(
            sender_balance.available_balance - Decimal::from(self.sum_credits_used),
            Decimal::from(0),
        );
        active_sender_balance.available_balance = sea_orm::Set(new_available_balance);

        active_sender_balance.save(db_conn).await?;

        let downgrade_user = match user::Entity::find()
            .filter(user::Column::Id.eq(sender_user_id))
            .one(db_conn)
            .await?
        {
            Some(x) => x,
            None => {
                warn!("No user was found with this sender id!");
                return Ok(());
            }
        };

        let downgrade_user_role = user_tier::Entity::find()
            .filter(user_tier::Column::Id.eq(downgrade_user.user_tier_id))
            .one(db_conn)
            .await?
            .context(format!(
                "The foreign key for the user's user_tier_id was not found! {:?}",
                downgrade_user.user_tier_id
            ))?;

        // Downgrade a user to premium - out of funds if there's less than 10$ in the account, and if the user was premium before
        if new_available_balance < Decimal::from(10u64) && downgrade_user_role.title == "Premium" {
            let mut active_downgrade_user = downgrade_user.into_active_model();
            active_downgrade_user.user_tier_id = sea_orm::Set(downgrade_user_role.id);
            active_downgrade_user.save(db_conn).await?;
        }

        // Get the referee, and the referrer
        // (2) Look up the code that this user used. This is the referee table
        let referee_object = match referee::Entity::find()
            .filter(referee::Column::UserId.eq(sender_user_id))
            .one(db_conn)
            .await?
        {
            Some(x) => x,
            None => {
                warn!(
                    "No referral code was found for this user: {:?}",
                    sender_user_id
                );
                return Ok(());
            }
        };

        // (3) Look up the matching referrer in the referrer table
        // Referral table -> Get the referee id
        let user_with_that_referral_code = match referrer::Entity::find()
            .filter(referrer::Column::ReferralCode.eq(&referee_object.used_referral_code))
            .one(db_conn)
            .await?
        {
            Some(x) => x,
            None => {
                warn!(
                    "No referrer with that referral code was found {:?}",
                    referee_object
                );
                return Ok(());
            }
        };

        // Ok, now we add the credits to both users if applicable...
        // (4 onwards) Add balance to the referrer,

        // (5) Check if referee has used up $100.00 USD in total (Have a config item that says how many credits account to 1$)
        // Get balance for the referrer (optionally make it into an active model ...)
        let sender_balance = match balance::Entity::find()
            .filter(balance::Column::UserId.eq(referee_object.user_id))
            .one(db_conn)
            .await?
        {
            Some(x) => x,
            None => {
                warn!(
                    "This user id has no balance entry! {:?}",
                    referee_object.user_id
                );
                return Ok(());
            }
        };

        let mut active_sender_balance = sender_balance.clone().into_active_model();
        let referrer_balance = match balance::Entity::find()
            .filter(balance::Column::UserId.eq(user_with_that_referral_code.user_id))
            .one(db_conn)
            .await?
        {
            Some(x) => x,
            None => {
                warn!(
                    "This user id has no balance entry! {:?}",
                    referee_object.user_id
                );
                return Ok(());
            }
        };

        // I could try to circumvene the clone here, but let's skip that for now
        let mut active_referee = referee_object.clone().into_active_model();

        // (5.1) If not, go to (7). If yes, go to (6)
        // Hardcode this parameter also in config, so it's easier to tune
        if !referee_object.credits_applied_for_referee
            && (sender_balance.used_balance + self.sum_credits_used) >= Decimal::from(100)
        {
            // (6) If the credits have not yet been applied to the referee, apply 10M credits / $100.00 USD worth of credits.
            // Make it into an active model, and add credits
            active_sender_balance.available_balance =
                sea_orm::Set(sender_balance.available_balance + Decimal::from(100));
            // Also mark referral as "credits_applied_for_referee"
            active_referee.credits_applied_for_referee = sea_orm::Set(true);
        }

        // (7) If the referral-start-date has not been passed, apply 10% of the credits to the referrer.
        let now = Utc::now();
        let valid_until = DateTime::<Utc>::from_utc(referee_object.referral_start_date, Utc)
            .checked_add_months(Months::new(12))
            .unwrap();
        if now <= valid_until {
            let mut active_referrer_balance = referrer_balance.clone().into_active_model();
            // Add 10% referral fees ...
            active_referrer_balance.available_balance = sea_orm::Set(
                referrer_balance.available_balance
                    + Decimal::from(self.sum_credits_used / Decimal::from(10)),
            );
            // Also record how much the current referrer has "provided" / "gifted" away
            active_referee.credits_applied_for_referrer =
                sea_orm::Set(referee_object.credits_applied_for_referrer + self.sum_credits_used);
            active_referrer_balance.save(db_conn).await?;
        }

        active_sender_balance.save(db_conn).await?;
        active_referee.save(db_conn).await?;

        Ok(())
    }

    // TODO: change this to return a DataPoint?
    async fn save_timeseries(
        self,
        bucket: &str,
        measurement: &str,
        chain_id: u64,
        influxdb2_clent: &influxdb2::Client,
        key: RpcQueryKey,
    ) -> anyhow::Result<()> {
        // TODO: error if key.origin is set?

        // TODO: what name?
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
        let timestamp_precision = TimestampPrecision::Seconds;

        let points = [builder.build()?];

        // TODO: bucket should be an enum so that we don't risk typos
        influxdb2_clent
            .write_with_precision(bucket, stream::iter(points), timestamp_precision)
            .await?;

        Ok(())
    }
}

impl RpcQueryStats {
    pub fn new(
        method: Option<String>,
        authorization: Arc<Authorization>,
        metadata: Arc<RequestMetadata>,
        response_bytes: usize,
    ) -> Self {
        // TODO: try_unwrap the metadata to be sure that all the stats for this request have been collected
        // TODO: otherwise, i think the whole thing should be in a single lock that we can "reset" when a stat is created

        let archive_request = metadata.archive_request.load(Ordering::Acquire);
        let backend_requests = metadata.backend_requests.lock().len() as u64;
        let request_bytes = metadata.request_bytes;
        let error_response = metadata.error_response.load(Ordering::Acquire);
        let response_millis = metadata.start_instant.elapsed().as_millis() as u64;
        let response_bytes = response_bytes as u64;

        // TODO: Gotta make the arithmetic here

        // TODO: Depending on the method, metadata and response bytes, pick a different number of credits used
        // This can be a slightly more complex function as we ll
        // TODO: Here, let's implement the formula
        let credits_used = Self::compute_cost(
            request_bytes,
            response_bytes,
            backend_requests == 0,
            &method,
        );

        let response_timestamp = Utc::now().timestamp();

        Self {
            authorization,
            archive_request,
            method,
            backend_requests,
            request_bytes,
            error_response,
            response_bytes,
            response_millis,
            response_timestamp,
            credits_used,
        }
    }

    /// Compute cost per request
    /// All methods cost the same
    /// The number of bytes are based on input, and output bytes
    pub fn compute_cost(
        request_bytes: u64,
        response_bytes: u64,
        cache_hit: bool,
        _method: &Option<String>,
    ) -> Decimal {
        // TODO: Should make these lazy_static const?
        // pays at least $0.000018 / credits per request
        let cost_minimum = Decimal::new(18, 6);
        // 1kb is included on each call
        let cost_free_bytes = 1024;
        // after that, we add cost per bytes, $0.000000006 / credits per byte
        let cost_per_byte = Decimal::new(6, 9);

        let total_bytes = request_bytes + response_bytes;
        let total_chargable_bytes =
            Decimal::from(max(0, total_bytes as i64 - cost_free_bytes as i64));

        let out = cost_minimum + cost_per_byte * total_chargable_bytes;
        if cache_hit {
            out * Decimal::new(5, 1)
        } else {
            out
        }
    }

    /// Only used for migration from stats_v1 to stats_v2/v3
    pub fn modify_struct(
        &mut self,
        response_millis: u64,
        response_timestamp: i64,
        backend_requests: u64,
    ) {
        self.response_millis = response_millis;
        self.response_timestamp = response_timestamp;
        self.backend_requests = backend_requests;
    }
}

impl StatBuffer {
    #[allow(clippy::too_many_arguments)]
    pub fn try_spawn(
        chain_id: u64,
        bucket: String,
        db_conn: Option<DatabaseConnection>,
        influxdb_client: Option<influxdb2::Client>,
        db_save_interval_seconds: u32,
        tsdb_save_interval_seconds: u32,
        billing_period_seconds: i64,
        shutdown_receiver: broadcast::Receiver<()>,
    ) -> anyhow::Result<Option<SpawnedStatBuffer>> {
        if db_conn.is_none() && influxdb_client.is_none() {
            return Ok(None);
        }

        let (stat_sender, stat_receiver) = flume::unbounded();

        let mut new = Self {
            chain_id,
            db_conn,
            influxdb_client,
            db_save_interval_seconds,
            tsdb_save_interval_seconds,
            billing_period_seconds,
        };

        // any errors inside this task will cause the application to exit
        let handle = tokio::spawn(async move {
            new.aggregate_and_save_loop(bucket, stat_receiver, shutdown_receiver)
                .await
        });

        Ok(Some((stat_sender, handle).into()))
    }

    async fn aggregate_and_save_loop(
        &mut self,
        bucket: String,
        stat_receiver: flume::Receiver<AppStat>,
        mut shutdown_receiver: broadcast::Receiver<()>,
    ) -> anyhow::Result<()> {
        let mut tsdb_save_interval =
            interval(Duration::from_secs(self.tsdb_save_interval_seconds as u64));
        let mut db_save_interval =
            interval(Duration::from_secs(self.db_save_interval_seconds as u64));

        // TODO: this is used for rpc_accounting_v2 and influxdb. give it a name to match that? "stat" of some kind?
        let mut global_timeseries_buffer = HashMap::<RpcQueryKey, BufferedRpcQueryStats>::new();
        let mut opt_in_timeseries_buffer = HashMap::<RpcQueryKey, BufferedRpcQueryStats>::new();
        let mut accounting_db_buffer = HashMap::<RpcQueryKey, BufferedRpcQueryStats>::new();

        // TODO: Somewhere here we should probably be updating the balance of the user
        // And also update the credits used etc. for the referred user
        loop {
            tokio::select! {
                stat = stat_receiver.recv_async() => {
                    // info!("Received stat");
                    // save the stat to a buffer
                    match stat {
                        Ok(AppStat::RpcQuery(stat)) => {
                            if self.influxdb_client.is_some() {
                                // TODO: round the timestamp at all?

                                let global_timeseries_key = stat.global_timeseries_key();

                                global_timeseries_buffer.entry(global_timeseries_key).or_default().add(stat.clone());

                                let opt_in_timeseries_key =  stat.opt_in_timeseries_key();

                                opt_in_timeseries_buffer.entry(opt_in_timeseries_key).or_default().add(stat.clone());
                            }

                            if self.db_conn.is_some() {
                                accounting_db_buffer.entry(stat.accounting_key(self.billing_period_seconds)).or_default().add(stat);
                            }
                        }
                        Err(err) => {
                            error!("error receiving stat: {:?}", err);
                            break;
                        }
                    }
                }
                _ = db_save_interval.tick() => {
                    // info!("DB save internal tick");
                    let db_conn = self.db_conn.as_ref().expect("db connection should always exist if there are buffered stats");

                    // TODO: batch saves
                    for (key, stat) in accounting_db_buffer.drain() {
                        // TODO: i don't like passing key (which came from the stat) to the function on the stat. but it works for now
                        if let Err(err) = stat.save_db(self.chain_id, db_conn, key).await {
                            error!("unable to save accounting entry! err={:?}", err);
                        };
                    }
                }
                _ = tsdb_save_interval.tick() => {
                    // info!("TSDB save internal tick");
                    // TODO: batch saves
                    // TODO: better bucket names
                    let influxdb_client = self.influxdb_client.as_ref().expect("influxdb client should always exist if there are buffered stats");

                    for (key, stat) in global_timeseries_buffer.drain() {
                        // TODO: i don't like passing key (which came from the stat) to the function on the stat. but it works for now
                        if let Err(err) = stat.save_timeseries(bucket.clone().as_ref(), "global_proxy", self.chain_id, influxdb_client, key).await {
                            error!("unable to save global stat! err={:?}", err);
                        };
                    }

                    for (key, stat) in opt_in_timeseries_buffer.drain() {
                        // TODO: i don't like passing key (which came from the stat) to the function on the stat. but it works for now
                        if let Err(err) = stat.save_timeseries(bucket.clone().as_ref(), "opt_in_proxy", self.chain_id, influxdb_client, key).await {
                            error!("unable to save opt-in stat! err={:?}", err);
                        };
                    }
                }
                x = shutdown_receiver.recv() => {
                    info!("shutdown signal ---");
                    match x {
                        Ok(_) => {
                            info!("stat_loop shutting down");
                        },
                        Err(err) => error!("stat_loop shutdown receiver err={:?}", err),
                    }
                    break;
                }
            }
        }

        // TODO: dry
        if let Some(db_conn) = self.db_conn.as_ref() {
            info!(
                "saving {} buffered accounting entries",
                accounting_db_buffer.len(),
            );

            for (key, stat) in accounting_db_buffer.drain() {
                if let Err(err) = stat.save_db(self.chain_id, db_conn, key).await {
                    error!(
                        "Unable to save accounting entry while shutting down! err={:?}",
                        err
                    );
                };
            }
        }

        // TODO: dry
        if let Some(influxdb_client) = self.influxdb_client.as_ref() {
            info!(
                "saving {} buffered global stats",
                global_timeseries_buffer.len(),
            );

            for (key, stat) in global_timeseries_buffer.drain() {
                if let Err(err) = stat
                    .save_timeseries(&bucket, "global_proxy", self.chain_id, influxdb_client, key)
                    .await
                {
                    error!(
                        "Unable to save global stat while shutting down! err={:?}",
                        err
                    );
                };
            }

            info!(
                "saving {} buffered opt-in stats",
                opt_in_timeseries_buffer.len(),
            );

            for (key, stat) in opt_in_timeseries_buffer.drain() {
                if let Err(err) = stat
                    .save_timeseries(&bucket, "opt_in_proxy", self.chain_id, influxdb_client, key)
                    .await
                {
                    error!(
                        "unable to save opt-in stat while shutting down! err={:?}",
                        err
                    );
                };
            }
        }

        info!("accounting and stat save loop complete");

        Ok(())
    }
}
