//! Utilities for authorization of logged in and anonymous users.

use super::errors::FrontendErrorResponse;
use crate::app::{AuthorizationChecks, Web3ProxyApp, APP_USER_AGENT};
use crate::user_token::UserBearerToken;
use anyhow::Context;
use axum::headers::authorization::Bearer;
use axum::headers::{Header, Origin, Referer, UserAgent};
use chrono::Utc;
use deferred_rate_limiter::DeferredRateLimitResult;
use entities::{login, rpc_key, user, user_tier};
use hashbrown::HashMap;
use http::HeaderValue;
use ipnet::IpNet;
use log::error;
use migration::sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use redis_rate_limiter::RedisRateLimitResult;
use std::fmt::Display;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::{net::IpAddr, str::FromStr, sync::Arc};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use ulid::Ulid;
use uuid::Uuid;

/// This lets us use UUID and ULID while we transition to only ULIDs
/// TODO: include the key's description.
#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum RpcSecretKey {
    Ulid(Ulid),
    Uuid(Uuid),
}

/// TODO: should this have IpAddr and Origin or AuthorizationChecks?
#[derive(Debug)]
pub enum RateLimitResult {
    Allowed(Authorization, Option<OwnedSemaphorePermit>),
    RateLimited(
        Authorization,
        /// when their rate limit resets and they can try more requests
        Option<Instant>,
    ),
    /// This key is not in our database. Deny access!
    UnknownKey,
}

#[derive(Clone, Debug)]
pub enum AuthorizationType {
    Internal,
    Frontend,
}

/// TODO: include the authorization checks in this?
#[derive(Clone, Debug)]
pub struct Authorization {
    pub checks: AuthorizationChecks,
    // TODO: instead of the conn, have a channel?
    pub db_conn: Option<DatabaseConnection>,
    pub ip: IpAddr,
    pub origin: Option<Origin>,
    pub referer: Option<Referer>,
    pub user_agent: Option<UserAgent>,
    pub authorization_type: AuthorizationType,
}

#[derive(Debug)]
pub struct RequestMetadata {
    pub start_datetime: chrono::DateTime<Utc>,
    pub start_instant: tokio::time::Instant,
    // TODO: better name for this
    pub period_seconds: u64,
    pub request_bytes: u64,
    // TODO: do we need atomics? seems like we should be able to pass a &mut around
    // TODO: "archive" isn't really a boolean.
    pub archive_request: AtomicBool,
    /// if this is 0, there was a cache_hit
    pub backend_requests: AtomicU64,
    pub no_servers: AtomicU64,
    pub error_response: AtomicBool,
    pub response_bytes: AtomicU64,
    pub response_millis: AtomicU64,
}

impl RequestMetadata {
    pub fn new(period_seconds: u64, request_bytes: usize) -> anyhow::Result<Self> {
        // TODO: how can we do this without turning it into a string first. this is going to slow us down!
        let request_bytes = request_bytes as u64;

        let new = Self {
            start_instant: Instant::now(),
            start_datetime: Utc::now(),
            period_seconds,
            request_bytes,
            archive_request: false.into(),
            backend_requests: 0.into(),
            no_servers: 0.into(),
            error_response: false.into(),
            response_bytes: 0.into(),
            response_millis: 0.into(),
        };

        Ok(new)
    }
}

impl RpcSecretKey {
    pub fn new() -> Self {
        Ulid::new().into()
    }
}

impl Default for RpcSecretKey {
    fn default() -> Self {
        Self::new()
    }
}

impl Display for RpcSecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // TODO: do this without dereferencing
        let ulid: Ulid = (*self).into();

        ulid.fmt(f)
    }
}

impl FromStr for RpcSecretKey {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(ulid) = s.parse::<Ulid>() {
            Ok(ulid.into())
        } else if let Ok(uuid) = s.parse::<Uuid>() {
            Ok(uuid.into())
        } else {
            // TODO: custom error type so that this shows as a 400
            Err(anyhow::anyhow!("UserKey was not a ULID or UUID"))
        }
    }
}

impl From<Ulid> for RpcSecretKey {
    fn from(x: Ulid) -> Self {
        RpcSecretKey::Ulid(x)
    }
}

impl From<Uuid> for RpcSecretKey {
    fn from(x: Uuid) -> Self {
        RpcSecretKey::Uuid(x)
    }
}

impl From<RpcSecretKey> for Ulid {
    fn from(x: RpcSecretKey) -> Self {
        match x {
            RpcSecretKey::Ulid(x) => x,
            RpcSecretKey::Uuid(x) => Ulid::from(x.as_u128()),
        }
    }
}

impl From<RpcSecretKey> for Uuid {
    fn from(x: RpcSecretKey) -> Self {
        match x {
            RpcSecretKey::Ulid(x) => Uuid::from_u128(x.0),
            RpcSecretKey::Uuid(x) => x,
        }
    }
}

impl Authorization {
    pub fn internal(db_conn: Option<DatabaseConnection>) -> anyhow::Result<Self> {
        let authorization_checks = AuthorizationChecks {
            // any error logs on a local (internal) query are likely problems. log them all
            log_revert_chance: 1.0,
            // default for everything else should be fine. we don't have a user_id or ip to give
            ..Default::default()
        };

        let ip: IpAddr = "127.0.0.1".parse().expect("localhost should always parse");
        let user_agent = UserAgent::from_str(APP_USER_AGENT).ok();

        Self::try_new(
            authorization_checks,
            db_conn,
            ip,
            None,
            None,
            user_agent,
            AuthorizationType::Internal,
        )
    }

    pub fn external(
        allowed_origin_requests_per_period: &HashMap<String, u64>,
        db_conn: Option<DatabaseConnection>,
        ip: IpAddr,
        origin: Option<Origin>,
        referer: Option<Referer>,
        user_agent: Option<UserAgent>,
    ) -> anyhow::Result<Self> {
        // some origins can override max_requests_per_period for anon users
        let max_requests_per_period = origin
            .as_ref()
            .map(|origin| {
                allowed_origin_requests_per_period
                    .get(&origin.to_string())
                    .cloned()
            })
            .unwrap_or_default();

        // TODO: default or None?
        let authorization_checks = AuthorizationChecks {
            max_requests_per_period,
            ..Default::default()
        };

        Self::try_new(
            authorization_checks,
            db_conn,
            ip,
            origin,
            referer,
            user_agent,
            AuthorizationType::Frontend,
        )
    }

    pub fn try_new(
        authorization_checks: AuthorizationChecks,
        db_conn: Option<DatabaseConnection>,
        ip: IpAddr,
        origin: Option<Origin>,
        referer: Option<Referer>,
        user_agent: Option<UserAgent>,
        authorization_type: AuthorizationType,
    ) -> anyhow::Result<Self> {
        // check ip
        match &authorization_checks.allowed_ips {
            None => {}
            Some(allowed_ips) => {
                if !allowed_ips.iter().any(|x| x.contains(&ip)) {
                    return Err(anyhow::anyhow!("IP is not allowed!"));
                }
            }
        }

        // check origin
        match (&origin, &authorization_checks.allowed_origins) {
            (None, None) => {}
            (Some(_), None) => {}
            (None, Some(_)) => return Err(anyhow::anyhow!("Origin required")),
            (Some(origin), Some(allowed_origins)) => {
                if !allowed_origins.contains(origin) {
                    return Err(anyhow::anyhow!("IP is not allowed!"));
                }
            }
        }

        // check referer
        match (&referer, &authorization_checks.allowed_referers) {
            (None, None) => {}
            (Some(_), None) => {}
            (None, Some(_)) => return Err(anyhow::anyhow!("Referer required")),
            (Some(referer), Some(allowed_referers)) => {
                if !allowed_referers.contains(referer) {
                    return Err(anyhow::anyhow!("Referer is not allowed!"));
                }
            }
        }

        // check user_agent
        match (&user_agent, &authorization_checks.allowed_user_agents) {
            (None, None) => {}
            (Some(_), None) => {}
            (None, Some(_)) => return Err(anyhow::anyhow!("User agent required")),
            (Some(user_agent), Some(allowed_user_agents)) => {
                if !allowed_user_agents.contains(user_agent) {
                    return Err(anyhow::anyhow!("User agent is not allowed!"));
                }
            }
        }

        Ok(Self {
            checks: authorization_checks,
            db_conn,
            ip,
            origin,
            referer,
            user_agent,
            authorization_type,
        })
    }
}

/// rate limit logins only by ip.
/// we want all origins and referers and user agents to count together
pub async fn login_is_authorized(
    app: &Web3ProxyApp,
    ip: IpAddr,
) -> Result<Authorization, FrontendErrorResponse> {
    let authorization = match app.rate_limit_login(ip).await? {
        RateLimitResult::Allowed(authorization, None) => authorization,
        RateLimitResult::RateLimited(authorization, retry_at) => {
            return Err(FrontendErrorResponse::RateLimited(authorization, retry_at));
        }
        // TODO: don't panic. give the user an error
        x => unimplemented!("rate_limit_login shouldn't ever see these: {:?}", x),
    };

    Ok(authorization)
}

/// semaphore won't ever be None, but its easier if key auth and ip auth work the same way
pub async fn ip_is_authorized(
    app: &Web3ProxyApp,
    ip: IpAddr,
    origin: Option<Origin>,
) -> Result<(Authorization, Option<OwnedSemaphorePermit>), FrontendErrorResponse> {
    // TODO: i think we could write an `impl From` for this
    // TODO: move this to an AuthorizedUser extrator
    let (authorization, semaphore) = match app
        .rate_limit_by_ip(&app.config.allowed_origin_requests_per_period, ip, origin)
        .await?
    {
        RateLimitResult::Allowed(authorization, semaphore) => (authorization, semaphore),
        RateLimitResult::RateLimited(authorization, retry_at) => {
            return Err(FrontendErrorResponse::RateLimited(authorization, retry_at));
        }
        // TODO: don't panic. give the user an error
        x => unimplemented!("rate_limit_by_ip shouldn't ever see these: {:?}", x),
    };

    Ok((authorization, semaphore))
}

/// like app.rate_limit_by_rpc_key but converts to a FrontendErrorResponse;
pub async fn key_is_authorized(
    app: &Web3ProxyApp,
    rpc_key: RpcSecretKey,
    ip: IpAddr,
    origin: Option<Origin>,
    referer: Option<Referer>,
    user_agent: Option<UserAgent>,
) -> Result<(Authorization, Option<OwnedSemaphorePermit>), FrontendErrorResponse> {
    // check the rate limits. error if over the limit
    // TODO: i think this should be in an "impl From" or "impl Into"
    let (authorization, semaphore) = match app
        .rate_limit_by_rpc_key(ip, origin, referer, rpc_key, user_agent)
        .await?
    {
        RateLimitResult::Allowed(authorization, semaphore) => (authorization, semaphore),
        RateLimitResult::RateLimited(authorization, retry_at) => {
            return Err(FrontendErrorResponse::RateLimited(authorization, retry_at));
        }
        RateLimitResult::UnknownKey => return Err(FrontendErrorResponse::UnknownKey),
    };

    Ok((authorization, semaphore))
}

impl Web3ProxyApp {
    /// Limit the number of concurrent requests from the given ip address.
    pub async fn ip_semaphore(&self, ip: IpAddr) -> anyhow::Result<Option<OwnedSemaphorePermit>> {
        if let Some(max_concurrent_requests) = self.config.public_max_concurrent_requests {
            let semaphore = self
                .ip_semaphores
                .get_with(ip, async move {
                    // TODO: set max_concurrent_requests dynamically based on load?
                    let s = Semaphore::new(max_concurrent_requests);
                    Arc::new(s)
                })
                .await;

            // if semaphore.available_permits() == 0 {
            //     // TODO: concurrent limit hit! emit a stat? less important for anon users
            //     // TODO: there is probably a race here
            // }

            let semaphore_permit = semaphore.acquire_owned().await?;

            Ok(Some(semaphore_permit))
        } else {
            Ok(None)
        }
    }

    /// Limit the number of concurrent requests from the given rpc key.
    pub async fn rpc_key_semaphore(
        &self,
        authorization_checks: &AuthorizationChecks,
    ) -> anyhow::Result<Option<OwnedSemaphorePermit>> {
        if let Some(max_concurrent_requests) = authorization_checks.max_concurrent_requests {
            let rpc_key_id = authorization_checks.rpc_key_id.context("no rpc_key_id")?;

            let semaphore = self
                .rpc_key_semaphores
                .get_with(rpc_key_id, async move {
                    let s = Semaphore::new(max_concurrent_requests as usize);
                    // // trace!("new semaphore for rpc_key_id {}", rpc_key_id);
                    Arc::new(s)
                })
                .await;

            // if semaphore.available_permits() == 0 {
            //     // TODO: concurrent limit hit! emit a stat? this has a race condition though.
            //     // TODO: maybe have a stat on how long we wait to acquire the semaphore instead?
            // }

            let semaphore_permit = semaphore.acquire_owned().await?;

            Ok(Some(semaphore_permit))
        } else {
            // unlimited requests allowed
            Ok(None)
        }
    }

    /// Verify that the given bearer token and address are allowed to take the specified action.
    /// This includes concurrent request limiting.
    pub async fn bearer_is_authorized(
        &self,
        bearer: Bearer,
    ) -> Result<(user::Model, OwnedSemaphorePermit), FrontendErrorResponse> {
        // get the user id for this bearer token
        let user_bearer_token = UserBearerToken::try_from(bearer)?;

        // limit concurrent requests
        let semaphore = self
            .bearer_token_semaphores
            .get_with(user_bearer_token.clone(), async move {
                let s = Semaphore::new(self.config.bearer_token_max_concurrent_requests as usize);
                Arc::new(s)
            })
            .await;

        let semaphore_permit = semaphore.acquire_owned().await?;

        // get the attached address from the database for the given auth_token.
        let db_replica = self
            .db_replica()
            .context("checking if bearer token is authorized")?;

        let user_bearer_uuid: Uuid = user_bearer_token.into();

        let user = user::Entity::find()
            .left_join(login::Entity)
            .filter(login::Column::BearerToken.eq(user_bearer_uuid))
            .one(db_replica.conn())
            .await
            .context("fetching user from db by bearer token")?
            .context("unknown bearer token")?;

        Ok((user, semaphore_permit))
    }

    pub async fn rate_limit_login(&self, ip: IpAddr) -> anyhow::Result<RateLimitResult> {
        // TODO: dry this up with rate_limit_by_rpc_key?

        // we don't care about user agent or origin or referer
        let authorization = Authorization::external(
            &self.config.allowed_origin_requests_per_period,
            self.db_conn(),
            ip,
            None,
            None,
            None,
        )?;

        // no semaphore is needed here because login rate limits are low
        // TODO: are we sure do we want a semaphore here?
        let semaphore = None;

        if let Some(rate_limiter) = &self.login_rate_limiter {
            match rate_limiter.throttle_label(&ip.to_string(), None, 1).await {
                Ok(RedisRateLimitResult::Allowed(_)) => {
                    Ok(RateLimitResult::Allowed(authorization, semaphore))
                }
                Ok(RedisRateLimitResult::RetryAt(retry_at, _)) => {
                    // TODO: set headers so they know when they can retry
                    // TODO: debug or trace?
                    // this is too verbose, but a stat might be good
                    // // trace!(?ip, "login rate limit exceeded until {:?}", retry_at);

                    Ok(RateLimitResult::RateLimited(authorization, Some(retry_at)))
                }
                Ok(RedisRateLimitResult::RetryNever) => {
                    // TODO: i don't think we'll get here. maybe if we ban an IP forever? seems unlikely
                    // // trace!(?ip, "login rate limit is 0");
                    Ok(RateLimitResult::RateLimited(authorization, None))
                }
                Err(err) => {
                    // internal error, not rate limit being hit
                    // TODO: i really want axum to do this for us in a single place.
                    error!("login rate limiter is unhappy. allowing ip. err={:?}", err);

                    Ok(RateLimitResult::Allowed(authorization, None))
                }
            }
        } else {
            // TODO: if no redis, rate limit with a local cache? "warn!" probably isn't right
            Ok(RateLimitResult::Allowed(authorization, None))
        }
    }

    /// origin is included because it can override the default rate limits
    pub async fn rate_limit_by_ip(
        &self,
        allowed_origin_requests_per_period: &HashMap<String, u64>,
        ip: IpAddr,
        origin: Option<Origin>,
    ) -> anyhow::Result<RateLimitResult> {
        // ip rate limits don't check referer or user agent
        // the do check
        let authorization = Authorization::external(
            allowed_origin_requests_per_period,
            self.db_conn.clone(),
            ip,
            origin,
            None,
            None,
        )?;

        if let Some(rate_limiter) = &self.frontend_ip_rate_limiter {
            match rate_limiter
                .throttle(ip, authorization.checks.max_requests_per_period, 1)
                .await
            {
                Ok(DeferredRateLimitResult::Allowed) => {
                    // rate limit allowed us. check concurrent request limits
                    let semaphore = self.ip_semaphore(ip).await?;

                    Ok(RateLimitResult::Allowed(authorization, semaphore))
                }
                Ok(DeferredRateLimitResult::RetryAt(retry_at)) => {
                    // TODO: set headers so they know when they can retry
                    // // trace!(?ip, "rate limit exceeded until {:?}", retry_at);
                    Ok(RateLimitResult::RateLimited(authorization, Some(retry_at)))
                }
                Ok(DeferredRateLimitResult::RetryNever) => {
                    // TODO: i don't think we'll get here. maybe if we ban an IP forever? seems unlikely
                    // // trace!(?ip, "rate limit is 0");
                    Ok(RateLimitResult::RateLimited(authorization, None))
                }
                Err(err) => {
                    // this an internal error of some kind, not the rate limit being hit
                    // TODO: i really want axum to do this for us in a single place.
                    error!("rate limiter is unhappy. allowing ip. err={:?}", err);

                    // at least we can still check the semaphore
                    let semaphore = self.ip_semaphore(ip).await?;

                    Ok(RateLimitResult::Allowed(authorization, semaphore))
                }
            }
        } else {
            // no redis, but we can still check the ip semaphore
            let semaphore = self.ip_semaphore(ip).await?;

            // TODO: if no redis, rate limit with a local cache? "warn!" probably isn't right
            Ok(RateLimitResult::Allowed(authorization, semaphore))
        }
    }

    // check the local cache for user data, or query the database
    pub(crate) async fn authorization_checks(
        &self,
        rpc_secret_key: RpcSecretKey,
    ) -> anyhow::Result<AuthorizationChecks> {
        let authorization_checks: Result<_, Arc<anyhow::Error>> = self
            .rpc_secret_key_cache
            .try_get_with(rpc_secret_key.into(), async move {
                // trace!(?rpc_secret_key, "user cache miss");

                let db_replica = self.db_replica().context("Getting database connection")?;

                let rpc_secret_key: Uuid = rpc_secret_key.into();

                // TODO: join the user table to this to return the User? we don't always need it
                // TODO: join on secondary users
                // TODO: join on user tier
                match rpc_key::Entity::find()
                    .filter(rpc_key::Column::SecretKey.eq(rpc_secret_key))
                    .filter(rpc_key::Column::Active.eq(true))
                    .one(db_replica.conn())
                    .await?
                {
                    Some(rpc_key_model) => {
                        // TODO: move these splits into helper functions
                        // TODO: can we have sea orm handle this for us?
                        let user_model = user::Entity::find_by_id(rpc_key_model.user_id)
                            .one(db_replica.conn())
                            .await?
                            .expect("related user");

                        let user_tier_model =
                            user_tier::Entity::find_by_id(user_model.user_tier_id)
                                .one(db_replica.conn())
                                .await?
                                .expect("related user tier");

                        let allowed_ips: Option<Vec<IpNet>> =
                            if let Some(allowed_ips) = rpc_key_model.allowed_ips {
                                let x = allowed_ips
                                    .split(',')
                                    .map(|x| x.parse::<IpNet>())
                                    .collect::<Result<Vec<_>, _>>()?;
                                Some(x)
                            } else {
                                None
                            };

                        let allowed_origins: Option<Vec<Origin>> =
                            if let Some(allowed_origins) = rpc_key_model.allowed_origins {
                                // TODO: do this without collecting twice?
                                let x = allowed_origins
                                    .split(',')
                                    .map(HeaderValue::from_str)
                                    .collect::<Result<Vec<_>, _>>()?
                                    .into_iter()
                                    .map(|x| Origin::decode(&mut [x].iter()))
                                    .collect::<Result<Vec<_>, _>>()?;

                                Some(x)
                            } else {
                                None
                            };

                        let allowed_referers: Option<Vec<Referer>> =
                            if let Some(allowed_referers) = rpc_key_model.allowed_referers {
                                let x = allowed_referers
                                    .split(',')
                                    .map(|x| x.parse::<Referer>())
                                    .collect::<Result<Vec<_>, _>>()?;

                                Some(x)
                            } else {
                                None
                            };

                        let allowed_user_agents: Option<Vec<UserAgent>> =
                            if let Some(allowed_user_agents) = rpc_key_model.allowed_user_agents {
                                let x: Result<Vec<_>, _> = allowed_user_agents
                                    .split(',')
                                    .map(|x| x.parse::<UserAgent>())
                                    .collect();

                                Some(x?)
                            } else {
                                None
                            };

                        let rpc_key_id =
                            Some(rpc_key_model.id.try_into().expect("db ids are never 0"));

                        Ok(AuthorizationChecks {
                            user_id: rpc_key_model.user_id,
                            rpc_key_id,
                            allowed_ips,
                            allowed_origins,
                            allowed_referers,
                            allowed_user_agents,
                            log_level: rpc_key_model.log_level,
                            log_revert_chance: rpc_key_model.log_revert_chance,
                            max_concurrent_requests: user_tier_model.max_concurrent_requests,
                            max_requests_per_period: user_tier_model.max_requests_per_period,
                        })
                    }
                    None => Ok(AuthorizationChecks::default()),
                }
            })
            .await;

        // TODO: what's the best way to handle this arc? try_unwrap will not work
        authorization_checks.map_err(|err| anyhow::anyhow!(err))
    }

    /// Authorized the ip/origin/referer/useragent and rate limit and concurrency
    pub async fn rate_limit_by_rpc_key(
        &self,
        ip: IpAddr,
        origin: Option<Origin>,
        referer: Option<Referer>,
        rpc_key: RpcSecretKey,
        user_agent: Option<UserAgent>,
    ) -> anyhow::Result<RateLimitResult> {
        let authorization_checks = self.authorization_checks(rpc_key).await?;

        // if no rpc_key_id matching the given rpc was found, then we can't rate limit by key
        if authorization_checks.rpc_key_id.is_none() {
            return Ok(RateLimitResult::UnknownKey);
        }

        // only allow this rpc_key to run a limited amount of concurrent requests
        // TODO: rate limit should be BEFORE the semaphore!
        let semaphore = self.rpc_key_semaphore(&authorization_checks).await?;

        let authorization = Authorization::try_new(
            authorization_checks,
            self.db_conn(),
            ip,
            origin,
            referer,
            user_agent,
            AuthorizationType::Frontend,
        )?;

        let user_max_requests_per_period = match authorization.checks.max_requests_per_period {
            None => {
                return Ok(RateLimitResult::Allowed(authorization, semaphore));
            }
            Some(x) => x,
        };

        // user key is valid. now check rate limits
        if let Some(rate_limiter) = &self.frontend_key_rate_limiter {
            match rate_limiter
                .throttle(rpc_key.into(), Some(user_max_requests_per_period), 1)
                .await
            {
                Ok(DeferredRateLimitResult::Allowed) => {
                    Ok(RateLimitResult::Allowed(authorization, semaphore))
                }
                Ok(DeferredRateLimitResult::RetryAt(retry_at)) => {
                    // TODO: set headers so they know when they can retry
                    // TODO: debug or trace?
                    // this is too verbose, but a stat might be good
                    // TODO: keys are secrets! use the id instead
                    // TODO: emit a stat
                    // // trace!(?rpc_key, "rate limit exceeded until {:?}", retry_at);
                    Ok(RateLimitResult::RateLimited(authorization, Some(retry_at)))
                }
                Ok(DeferredRateLimitResult::RetryNever) => {
                    // TODO: keys are secret. don't log them!
                    // // trace!(?rpc_key, "rate limit is 0");
                    // TODO: emit a stat
                    Ok(RateLimitResult::RateLimited(authorization, None))
                }
                Err(err) => {
                    // internal error, not rate limit being hit
                    // TODO: i really want axum to do this for us in a single place.
                    error!("rate limiter is unhappy. allowing ip. err={:?}", err);

                    Ok(RateLimitResult::Allowed(authorization, semaphore))
                }
            }
        } else {
            // TODO: if no redis, rate limit with just a local cache?
            Ok(RateLimitResult::Allowed(authorization, semaphore))
        }
    }
}
