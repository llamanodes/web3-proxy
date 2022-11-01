//! Utilities for authorization of logged in and anonymous users.

use super::errors::FrontendErrorResponse;
use crate::app::{UserKeyData, Web3ProxyApp};
use crate::jsonrpc::JsonRpcRequest;
use crate::user_token::UserBearerToken;
use anyhow::Context;
use axum::headers::authorization::Bearer;
use axum::headers::{Header, Origin, Referer, UserAgent};
use axum::TypedHeader;
use chrono::Utc;
use deferred_rate_limiter::DeferredRateLimitResult;
use entities::{rpc_key, user, user_tier};
use http::HeaderValue;
use ipnet::IpNet;
use redis_rate_limiter::redis::AsyncCommands;
use redis_rate_limiter::RedisRateLimitResult;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use std::fmt::Display;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::{net::IpAddr, str::FromStr, sync::Arc};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tracing::{error, instrument, trace};
use ulid::Ulid;
use uuid::Uuid;

/// This lets us use UUID and ULID while we transition to only ULIDs
/// TODO: include the key's description.
#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum RpcSecretKey {
    Ulid(Ulid),
    Uuid(Uuid),
}

#[derive(Debug)]
pub enum RateLimitResult {
    /// contains the IP of the anonymous user
    /// TODO: option inside or outside the arc?
    AllowedIp(IpAddr, Option<OwnedSemaphorePermit>),
    /// contains the rpc_key_id of an authenticated user
    AllowedUser(UserKeyData, Option<OwnedSemaphorePermit>),
    /// contains the IP and retry_at of the anonymous user
    RateLimitedIp(IpAddr, Option<Instant>),
    /// contains the rpc_key_id and retry_at of an authenticated user key
    RateLimitedUser(UserKeyData, Option<Instant>),
    /// This key is not in our database. Deny access!
    UnknownKey,
}

#[derive(Clone, Debug)]
pub struct AuthorizedKey {
    pub ip: IpAddr,
    pub origin: Option<Origin>,
    pub user_id: u64,
    pub rpc_key_id: u64,
    // TODO: just use an f32? even an f16 is probably fine
    pub log_revert_chance: f64,
}

#[derive(Debug)]
pub struct RequestMetadata {
    pub start_datetime: chrono::DateTime<Utc>,
    pub start_instant: tokio::time::Instant,
    // TODO: better name for this
    pub period_seconds: u64,
    pub request_bytes: u64,
    /// if this is 0, there was a cache_hit
    pub backend_requests: AtomicU64,
    pub no_servers: AtomicU64,
    pub error_response: AtomicBool,
    pub response_bytes: AtomicU64,
    pub response_millis: AtomicU64,
}

#[derive(Clone, Debug)]
pub enum AuthorizedRequest {
    /// Request from this app
    Internal,
    /// Request from an anonymous IP address
    Ip(IpAddr, Option<Origin>),
    /// Request from an authenticated and authorized user
    User(Option<DatabaseConnection>, AuthorizedKey),
}

impl RequestMetadata {
    pub fn new(period_seconds: u64, request: &JsonRpcRequest) -> anyhow::Result<Self> {
        // TODO: how can we do this without turning it into a string first. this is going to slow us down!
        let request_bytes = serde_json::to_string(request)
            .context("finding request size")?
            .len()
            .try_into()?;

        let new = Self {
            start_instant: Instant::now(),
            start_datetime: Utc::now(),
            period_seconds,
            request_bytes,
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

impl AuthorizedKey {
    pub fn try_new(
        ip: IpAddr,
        origin: Option<Origin>,
        referer: Option<Referer>,
        user_agent: Option<UserAgent>,
        rpc_key_data: UserKeyData,
    ) -> anyhow::Result<Self> {
        // check ip
        match &rpc_key_data.allowed_ips {
            None => {}
            Some(allowed_ips) => {
                if !allowed_ips.iter().any(|x| x.contains(&ip)) {
                    return Err(anyhow::anyhow!("IP is not allowed!"));
                }
            }
        }

        // check origin
        match (&origin, &rpc_key_data.allowed_origins) {
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
        match (referer, &rpc_key_data.allowed_referers) {
            (None, None) => {}
            (Some(_), None) => {}
            (None, Some(_)) => return Err(anyhow::anyhow!("Referer required")),
            (Some(referer), Some(allowed_referers)) => {
                if !allowed_referers.contains(&referer) {
                    return Err(anyhow::anyhow!("Referer is not allowed!"));
                }
            }
        }

        // check user_agent
        match (user_agent, &rpc_key_data.allowed_user_agents) {
            (None, None) => {}
            (Some(_), None) => {}
            (None, Some(_)) => return Err(anyhow::anyhow!("User agent required")),
            (Some(user_agent), Some(allowed_user_agents)) => {
                if !allowed_user_agents.contains(&user_agent) {
                    return Err(anyhow::anyhow!("User agent is not allowed!"));
                }
            }
        }

        Ok(Self {
            ip,
            origin,
            user_id: rpc_key_data.user_id,
            rpc_key_id: rpc_key_data.rpc_key_id,
            log_revert_chance: rpc_key_data.log_revert_chance,
        })
    }
}

impl AuthorizedRequest {
    /// Only User has a database connection in case it needs to save a revert to the database.
    pub fn db_conn(&self) -> Option<&DatabaseConnection> {
        match self {
            Self::User(x, _) => x.as_ref(),
            _ => None,
        }
    }
}

impl Display for &AuthorizedRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthorizedRequest::Internal => f.write_str("int"),
            AuthorizedRequest::Ip(x, _) => f.write_str(&format!("ip-{}", x)),
            AuthorizedRequest::User(_, x) => f.write_str(&format!("uk-{}", x.rpc_key_id)),
        }
    }
}

pub async fn login_is_authorized(
    app: &Web3ProxyApp,
    ip: IpAddr,
) -> Result<AuthorizedRequest, FrontendErrorResponse> {
    let (ip, _semaphore) = match app.rate_limit_login(ip).await? {
        RateLimitResult::AllowedIp(x, semaphore) => (x, semaphore),
        RateLimitResult::RateLimitedIp(x, retry_at) => {
            return Err(FrontendErrorResponse::RateLimitedIp(x, retry_at));
        }
        // TODO: don't panic. give the user an error
        x => unimplemented!("rate_limit_login shouldn't ever see these: {:?}", x),
    };

    Ok(AuthorizedRequest::Ip(ip, None))
}

pub async fn ip_is_authorized(
    app: &Web3ProxyApp,
    ip: IpAddr,
    origin: Option<TypedHeader<Origin>>,
) -> Result<(AuthorizedRequest, Option<OwnedSemaphorePermit>), FrontendErrorResponse> {
    let origin = origin.map(|x| x.0);

    // TODO: i think we could write an `impl From` for this
    // TODO: move this to an AuthorizedUser extrator
    let (ip, semaphore) = match app.rate_limit_by_ip(ip, origin.as_ref()).await? {
        RateLimitResult::AllowedIp(ip, semaphore) => (ip, semaphore),
        RateLimitResult::RateLimitedIp(x, retry_at) => {
            return Err(FrontendErrorResponse::RateLimitedIp(x, retry_at));
        }
        // TODO: don't panic. give the user an error
        x => unimplemented!("rate_limit_by_ip shouldn't ever see these: {:?}", x),
    };

    // semaphore won't ever be None, but its easier if key auth and ip auth work the same way
    Ok((AuthorizedRequest::Ip(ip, origin), semaphore))
}

pub async fn key_is_authorized(
    app: &Web3ProxyApp,
    rpc_key: RpcSecretKey,
    ip: IpAddr,
    origin: Option<Origin>,
    referer: Option<Referer>,
    user_agent: Option<UserAgent>,
) -> Result<(AuthorizedRequest, Option<OwnedSemaphorePermit>), FrontendErrorResponse> {
    // check the rate limits. error if over the limit
    let (user_data, semaphore) = match app.rate_limit_by_key(rpc_key).await? {
        RateLimitResult::AllowedUser(x, semaphore) => (x, semaphore),
        RateLimitResult::RateLimitedUser(x, retry_at) => {
            return Err(FrontendErrorResponse::RateLimitedUser(x, retry_at));
        }
        RateLimitResult::UnknownKey => return Err(FrontendErrorResponse::UnknownKey),
        // TODO: don't panic. give the user an error
        x => unimplemented!("rate_limit_by_key shouldn't ever see these: {:?}", x),
    };

    let authorized_user = AuthorizedKey::try_new(ip, origin, referer, user_agent, user_data)?;

    let db_conn = app.db_conn.clone();

    Ok((AuthorizedRequest::User(db_conn, authorized_user), semaphore))
}

impl Web3ProxyApp {
    /// Limit the number of concurrent requests from the given ip address.
    #[instrument(level = "trace")]
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

    /// Limit the number of concurrent requests from the given key address.
    #[instrument(level = "trace")]
    pub async fn user_rpc_key_semaphore(
        &self,
        rpc_key_data: &UserKeyData,
    ) -> anyhow::Result<Option<OwnedSemaphorePermit>> {
        if let Some(max_concurrent_requests) = rpc_key_data.max_concurrent_requests {
            let semaphore = self
                .rpc_key_semaphores
                .get_with(rpc_key_data.rpc_key_id, async move {
                    let s = Semaphore::new(max_concurrent_requests as usize);
                    trace!("new semaphore for rpc_key_id {}", rpc_key_data.rpc_key_id);
                    Arc::new(s)
                })
                .await;

            // if semaphore.available_permits() == 0 {
            //     // TODO: concurrent limit hit! emit a stat
            // }

            let semaphore_permit = semaphore.acquire_owned().await?;

            Ok(Some(semaphore_permit))
        } else {
            Ok(None)
        }
    }

    /// Verify that the given bearer token and address are allowed to take the specified action.
    /// This includes concurrent request limiting.
    #[instrument(level = "trace")]
    pub async fn bearer_is_authorized(
        &self,
        bearer: Bearer,
    ) -> anyhow::Result<(user::Model, OwnedSemaphorePermit)> {
        // limit concurrent requests
        let semaphore = self
            .bearer_token_semaphores
            .get_with(bearer.token().to_string(), async move {
                let s = Semaphore::new(self.config.bearer_token_max_concurrent_requests as usize);
                Arc::new(s)
            })
            .await;

        let semaphore_permit = semaphore.acquire_owned().await?;

        // get the user id for this bearer token
        // TODO: move redis key building to a helper function
        let bearer_cache_key = UserBearerToken::try_from(bearer)?.to_string();

        // get the attached address from redis for the given auth_token.
        let mut redis_conn = self.redis_conn().await?;

        let user_id: u64 = redis_conn
            .get::<_, Option<u64>>(bearer_cache_key)
            .await
            .context("fetching bearer cache key from redis")?
            .context("unknown bearer token")?;

        // turn user id into a user
        let db_conn = self.db_conn().context("Getting database connection")?;
        let user = user::Entity::find_by_id(user_id)
            .one(&db_conn)
            .await
            .context("fetching user from db by id")?
            .context("unknown user id")?;

        Ok((user, semaphore_permit))
    }

    #[instrument(level = "trace")]
    pub async fn rate_limit_login(&self, ip: IpAddr) -> anyhow::Result<RateLimitResult> {
        // TODO: dry this up with rate_limit_by_key
        // TODO: do we want a semaphore here?
        if let Some(rate_limiter) = &self.login_rate_limiter {
            match rate_limiter.throttle_label(&ip.to_string(), None, 1).await {
                Ok(RedisRateLimitResult::Allowed(_)) => Ok(RateLimitResult::AllowedIp(ip, None)),
                Ok(RedisRateLimitResult::RetryAt(retry_at, _)) => {
                    // TODO: set headers so they know when they can retry
                    // TODO: debug or trace?
                    // this is too verbose, but a stat might be good
                    trace!(?ip, "login rate limit exceeded until {:?}", retry_at);
                    Ok(RateLimitResult::RateLimitedIp(ip, Some(retry_at)))
                }
                Ok(RedisRateLimitResult::RetryNever) => {
                    // TODO: i don't think we'll get here. maybe if we ban an IP forever? seems unlikely
                    trace!(?ip, "login rate limit is 0");
                    Ok(RateLimitResult::RateLimitedIp(ip, None))
                }
                Err(err) => {
                    // internal error, not rate limit being hit
                    // TODO: i really want axum to do this for us in a single place.
                    error!(?err, "login rate limiter is unhappy. allowing ip");

                    Ok(RateLimitResult::AllowedIp(ip, None))
                }
            }
        } else {
            // TODO: if no redis, rate limit with a local cache? "warn!" probably isn't right
            Ok(RateLimitResult::AllowedIp(ip, None))
        }
    }

    #[instrument(level = "trace")]
    pub async fn rate_limit_by_ip(
        &self,
        ip: IpAddr,
        origin: Option<&Origin>,
    ) -> anyhow::Result<RateLimitResult> {
        // TODO: dry this up with rate_limit_by_key
        let semaphore = self.ip_semaphore(ip).await?;

        if let Some(rate_limiter) = &self.frontend_ip_rate_limiter {
            let max_requests_per_period = origin
                .map(|origin| {
                    self.config
                        .allowed_origin_requests_per_minute
                        .get(&origin.to_string())
                        .cloned()
                })
                .unwrap_or_default();

            match rate_limiter.throttle(ip, max_requests_per_period, 1).await {
                Ok(DeferredRateLimitResult::Allowed) => {
                    Ok(RateLimitResult::AllowedIp(ip, semaphore))
                }
                Ok(DeferredRateLimitResult::RetryAt(retry_at)) => {
                    // TODO: set headers so they know when they can retry
                    // TODO: debug or trace?
                    // this is too verbose, but a stat might be good
                    trace!(?ip, "rate limit exceeded until {:?}", retry_at);
                    Ok(RateLimitResult::RateLimitedIp(ip, Some(retry_at)))
                }
                Ok(DeferredRateLimitResult::RetryNever) => {
                    // TODO: i don't think we'll get here. maybe if we ban an IP forever? seems unlikely
                    trace!(?ip, "rate limit is 0");
                    Ok(RateLimitResult::RateLimitedIp(ip, None))
                }
                Err(err) => {
                    // internal error, not rate limit being hit
                    // TODO: i really want axum to do this for us in a single place.
                    error!(?err, "rate limiter is unhappy. allowing ip");

                    Ok(RateLimitResult::AllowedIp(ip, semaphore))
                }
            }
        } else {
            // TODO: if no redis, rate limit with a local cache? "warn!" probably isn't right
            Ok(RateLimitResult::AllowedIp(ip, semaphore))
        }
    }

    // check the local cache for user data, or query the database
    #[instrument(level = "trace")]
    pub(crate) async fn user_data(
        &self,
        rpc_secret_key: RpcSecretKey,
    ) -> anyhow::Result<UserKeyData> {
        let user_data: Result<_, Arc<anyhow::Error>> = self
            .rpc_secret_key_cache
            .try_get_with(rpc_secret_key.into(), async move {
                trace!(?rpc_secret_key, "user cache miss");

                let db_conn = self.db_conn().context("Getting database connection")?;

                let rpc_secret_key: Uuid = rpc_secret_key.into();

                // TODO: join the user table to this to return the User? we don't always need it
                // TODO: join on secondary users
                // TODO: join on user tier
                match rpc_key::Entity::find()
                    .filter(rpc_key::Column::SecretKey.eq(rpc_secret_key))
                    .filter(rpc_key::Column::Active.eq(true))
                    .one(&db_conn)
                    .await?
                {
                    Some(rpc_key_model) => {
                        // TODO: move these splits into helper functions
                        // TODO: can we have sea orm handle this for us?
                        // let user_tier_model = rpc_key_model.

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

                        // let user_tier_model = user_tier

                        Ok(UserKeyData {
                            user_id: rpc_key_model.user_id,
                            rpc_key_id: rpc_key_model.id,
                            allowed_ips,
                            allowed_origins,
                            allowed_referers,
                            allowed_user_agents,
                            log_revert_chance: rpc_key_model.log_revert_chance,
                            max_concurrent_requests: None, // todo! user_tier_model.max_concurrent_requests,
                            max_requests_per_period: None, // todo! user_tier_model.max_requests_per_period,
                        })
                    }
                    None => Ok(UserKeyData::default()),
                }
            })
            .await;

        // TODO: what's the best way to handle this arc? try_unwrap will not work
        user_data.map_err(|err| anyhow::anyhow!(err))
    }

    #[instrument(level = "trace")]
    pub async fn rate_limit_by_key(
        &self,
        rpc_key: RpcSecretKey,
    ) -> anyhow::Result<RateLimitResult> {
        let user_data = self.user_data(rpc_key).await?;

        if user_data.rpc_key_id == 0 {
            return Ok(RateLimitResult::UnknownKey);
        }

        let semaphore = self.user_rpc_key_semaphore(&user_data).await?;

        let user_max_requests_per_period = match user_data.max_requests_per_period {
            None => {
                return Ok(RateLimitResult::AllowedUser(user_data, semaphore));
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
                    Ok(RateLimitResult::AllowedUser(user_data, semaphore))
                }
                Ok(DeferredRateLimitResult::RetryAt(retry_at)) => {
                    // TODO: set headers so they know when they can retry
                    // TODO: debug or trace?
                    // this is too verbose, but a stat might be good
                    // TODO: keys are secrets! use the id instead
                    // TODO: emit a stat
                    trace!(?rpc_key, "rate limit exceeded until {:?}", retry_at);
                    Ok(RateLimitResult::RateLimitedUser(user_data, Some(retry_at)))
                }
                Ok(DeferredRateLimitResult::RetryNever) => {
                    // TODO: keys are secret. don't log them!
                    trace!(?rpc_key, "rate limit is 0");
                    // TODO: emit a stat
                    Ok(RateLimitResult::RateLimitedUser(user_data, None))
                }
                Err(err) => {
                    // internal error, not rate limit being hit
                    // TODO: i really want axum to do this for us in a single place.
                    error!(?err, "rate limiter is unhappy. allowing ip");

                    Ok(RateLimitResult::AllowedUser(user_data, semaphore))
                }
            }
        } else {
            // TODO: if no redis, rate limit with just a local cache?
            Ok(RateLimitResult::AllowedUser(user_data, semaphore))
        }
    }
}
