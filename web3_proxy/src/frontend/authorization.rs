//! Utilities for authorization of logged in and anonymous users.

use super::rpc_proxy_ws::ProxyMode;
use crate::app::{App, APP_USER_AGENT};
use crate::balance::Balance;
use crate::caches::RegisteredUserRateLimitKey;
use crate::errors::{Web3ProxyError, Web3ProxyErrorContext, Web3ProxyResult};
use crate::globals::global_db_replica_conn;
use crate::jsonrpc::{self, SingleRequest};
use crate::secrets::RpcSecretKey;
use crate::user_token::UserBearerToken;
use anyhow::Context;
use axum::headers::authorization::Bearer;
use axum::headers::{Header, Origin, Referer, UserAgent};
use chrono::Utc;
use deferred_rate_limiter::{DeferredRateLimitResult, DeferredRateLimiter};
use derive_more::From;
use entities::{login, rpc_key, user, user_tier};
use ethers::types::Bytes;
use ethers::utils::keccak256;
use futures::TryFutureExt;
use hashbrown::HashMap;
use http::HeaderValue;
use ipnet::IpNet;
use migration::sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use redis_rate_limiter::redis::AsyncCommands;
use redis_rate_limiter::{RedisRateLimitResult, RedisRateLimiter};
use serde::Serialize;
use serde_json::value::RawValue;
use std::borrow::Cow;
use std::fmt::{Debug, Display};
use std::hash::{Hash, Hasher};
use std::num::NonZeroU64;
use std::{net::IpAddr, str::FromStr, sync::Arc};
use tokio::sync::RwLock as AsyncRwLock;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tracing::{error, trace, warn};
use ulid::Ulid;
use uuid::Uuid;

/// TODO: should this have IpAddr and Origin or AuthorizationChecks?
#[derive(Debug)]
pub enum RateLimitResult {
    Allowed(Authorization),
    RateLimited(
        Authorization,
        /// when their rate limit resets and they can try more requests
        Option<Instant>,
    ),
    /// This key is not in our database. Deny access!
    UnknownKey,
}

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
pub enum AuthorizationType {
    /// TODO: sometimes localhost should be internal and other times it should be Frontend. make a better separatation
    Internal,
    Frontend,
}

/// TODO: move this
#[derive(Clone, Debug, Default, From)]
pub struct AuthorizationChecks {
    /// database id of the primary user. 0 if anon
    pub user_id: u64,
    /// locally cached balance that may drift slightly if the user is on multiple servers
    pub latest_balance: Arc<AsyncRwLock<Balance>>,
    /// the key used (if any)
    pub rpc_secret_key: Option<RpcSecretKey>,
    /// database id of the rpc key
    /// if this is None, then this request is being rate limited by ip
    pub rpc_secret_key_id: Option<NonZeroU64>,
    /// if None, allow unlimited queries. inherited from the user_tier
    pub max_requests_per_period: Option<u64>,
    // if None, allow unlimited concurrent requests. inherited from the user_tier
    pub max_concurrent_requests: Option<u32>,
    /// if None, allow any Origin
    pub allowed_origins: Option<Vec<Origin>>,
    /// if None, allow any Referer
    pub allowed_referers: Option<Vec<Referer>>,
    /// if None, allow any UserAgent
    pub allowed_user_agents: Option<Vec<UserAgent>>,
    /// if None, allow any IP Address
    pub allowed_ips: Option<Vec<IpNet>>,
    /// Chance to save reverting eth_call, eth_estimateGas, and eth_sendRawTransaction to the database.
    /// depending on the caller, errors might be expected. this keeps us from bloating our database
    /// u16::MAX == 100%
    pub log_revert_chance: u16,
    /// if true, transactions are broadcast only to private mempools.
    /// IMPORTANT! Once confirmed by a miner, they will be public on the blockchain!
    pub private_txs: bool,
    pub proxy_mode: ProxyMode,
    /// if the account had premium when this request metadata was created
    /// they might spend slightly more than they've paid, but we are okay with that
    /// TODO: we could price the request now and if its too high, downgrade. but thats more complex than we need
    pub paid_credits_used: bool,
}

/// TODO: include the authorization checks in this?
#[derive(Clone, Debug)]
pub struct Authorization {
    pub checks: AuthorizationChecks,
    pub ip: IpAddr,
    pub origin: Option<Origin>,
    pub referer: Option<Referer>,
    pub user_agent: Option<UserAgent>,
    pub authorization_type: AuthorizationType,
}

/// Ulids and Uuids matching the same bits hash the same
impl Hash for RpcSecretKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let x = self.as_128();

        x.hash(state);
    }
}

#[derive(Clone, Debug, Default, From, Serialize)]
pub enum RequestOrMethod {
    Request(SingleRequest),
    /// sometimes we don't have a full request. for example, when we are logging a websocket subscription
    Method(Cow<'static, str>, usize),
    #[default]
    None,
}

impl Default for Authorization {
    fn default() -> Self {
        Authorization::internal().unwrap()
    }
}

impl RequestOrMethod {
    pub fn id(&self) -> Box<RawValue> {
        match self {
            Self::Request(x) => x.id.clone(),
            Self::Method(_, _) => Default::default(),
            Self::None => Default::default(),
        }
    }

    pub fn method(&self) -> &str {
        match self {
            Self::Request(x) => x.method.as_ref(),
            Self::Method(x, _) => x,
            Self::None => "unknown",
        }
    }

    /// TODO: should this panic on Self::None|Self::Method?
    pub fn params(&self) -> &serde_json::Value {
        match self {
            Self::Request(x) => &x.params,
            Self::Method(..) => &serde_json::Value::Null,
            Self::None => &serde_json::Value::Null,
        }
    }

    pub fn jsonrpc_request(&self) -> Option<&SingleRequest> {
        match self {
            Self::Request(x) => Some(x),
            _ => None,
        }
    }

    pub fn num_bytes(&self) -> usize {
        match self {
            Self::Method(_, num_bytes) => *num_bytes,
            Self::Request(x) => x.num_bytes(),
            Self::None => 0,
        }
    }
}

// TODO: i think a trait is actually the right thing to use here
#[derive(From)]
pub enum ResponseOrBytes<'a> {
    Json(&'a serde_json::Value),
    Response(&'a jsonrpc::SingleResponse),
    Error(&'a Web3ProxyError),
    Bytes(u64),
}

impl ResponseOrBytes<'_> {
    pub fn num_bytes(&self) -> u64 {
        match self {
            Self::Json(x) => serde_json::to_string(x)
                .expect("this should always serialize")
                .len() as u64,
            Self::Response(x) => x.num_bytes(),
            Self::Bytes(num_bytes) => *num_bytes,
            Self::Error(x) => {
                let (_, x) = x.as_response_parts();

                x.num_bytes()
            }
        }
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

        Display::fmt(&ulid, f)
    }
}

impl FromStr for RpcSecretKey {
    type Err = Web3ProxyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(ulid) = s.parse::<Ulid>() {
            Ok(ulid.into())
        } else if let Ok(uuid) = s.parse::<Uuid>() {
            Ok(uuid.into())
        } else {
            // TODO: custom error type so that this shows as a 400
            Err(Web3ProxyError::InvalidUserKey)
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
    /// this acquires a read lock on the latest balance. Be careful not to deadlock!
    pub async fn active_premium(&self) -> bool {
        let user_balance = self.checks.latest_balance.read().await;

        user_balance.active_premium()
    }

    pub fn internal() -> Web3ProxyResult<Self> {
        let authorization_checks = AuthorizationChecks {
            // any error logs on a local (internal) query are likely problems. log them all
            log_revert_chance: 100,
            // default for everything else should be fine. we don't have a user_id or ip to give
            ..Default::default()
        };

        let ip: IpAddr = "127.0.0.1".parse().expect("localhost should always parse");
        let user_agent = UserAgent::from_str(APP_USER_AGENT).ok();

        Self::try_new(
            authorization_checks,
            &ip,
            None,
            None,
            user_agent.as_ref(),
            AuthorizationType::Internal,
        )
    }

    pub fn external(
        allowed_origin_requests_per_period: &HashMap<String, u64>,
        ip: &IpAddr,
        origin: Option<&Origin>,
        proxy_mode: ProxyMode,
        referer: Option<&Referer>,
        user_agent: Option<&UserAgent>,
    ) -> Web3ProxyResult<Self> {
        // some origins can override max_requests_per_period for anon users
        // TODO: i don't like the `to_string` here
        let max_requests_per_period = origin
            .map(|origin| {
                allowed_origin_requests_per_period
                    .get(&origin.to_string())
                    .cloned()
            })
            .unwrap_or_default();

        let authorization_checks = AuthorizationChecks {
            max_requests_per_period,
            proxy_mode,
            ..Default::default()
        };

        Self::try_new(
            authorization_checks,
            ip,
            origin,
            referer,
            user_agent,
            AuthorizationType::Frontend,
        )
    }

    pub async fn permit(&self, app: &App) -> Web3ProxyResult<Option<OwnedSemaphorePermit>> {
        match self.checks.rpc_secret_key_id {
            None => app.permit_public_concurrency(&self.ip).await,
            Some(_) => app.permit_premium_concurrency(self).await,
        }
    }

    pub fn try_new(
        authorization_checks: AuthorizationChecks,
        ip: &IpAddr,
        origin: Option<&Origin>,
        referer: Option<&Referer>,
        user_agent: Option<&UserAgent>,
        authorization_type: AuthorizationType,
    ) -> Web3ProxyResult<Self> {
        // check ip
        match &authorization_checks.allowed_ips {
            None => {}
            Some(allowed_ips) => {
                if !allowed_ips.iter().any(|x| x.contains(ip)) {
                    return Err(Web3ProxyError::IpNotAllowed(ip.to_owned()));
                }
            }
        }

        // check origin
        match (origin, &authorization_checks.allowed_origins) {
            (None, None) => {}
            (Some(_), None) => {}
            (None, Some(_)) => return Err(Web3ProxyError::OriginRequired),
            (Some(origin), Some(allowed_origins)) => {
                if !allowed_origins.contains(origin) {
                    return Err(Web3ProxyError::OriginNotAllowed(origin.to_owned()));
                }
            }
        }

        // check referer
        match (referer, &authorization_checks.allowed_referers) {
            (None, None) => {}
            (Some(_), None) => {}
            (None, Some(_)) => return Err(Web3ProxyError::RefererRequired),
            (Some(referer), Some(allowed_referers)) => {
                if !allowed_referers.contains(referer) {
                    return Err(Web3ProxyError::RefererNotAllowed(referer.to_owned()));
                }
            }
        }

        // check user_agent
        match (user_agent, &authorization_checks.allowed_user_agents) {
            (None, None) => {}
            (Some(_), None) => {}
            (None, Some(_)) => return Err(Web3ProxyError::UserAgentRequired),
            (Some(user_agent), Some(allowed_user_agents)) => {
                if !allowed_user_agents.contains(user_agent) {
                    return Err(Web3ProxyError::UserAgentNotAllowed(user_agent.to_owned()));
                }
            }
        }

        Ok(Self {
            checks: authorization_checks,
            ip: *ip,
            origin: origin.cloned(),
            referer: referer.cloned(),
            user_agent: user_agent.cloned(),
            authorization_type,
        })
    }
}

/// rate limit logins only by ip.
/// we want all origins and referers and user agents to count together
pub async fn login_is_authorized(app: &App, ip: IpAddr) -> Web3ProxyResult<Authorization> {
    let authorization = match app.rate_limit_login(ip, ProxyMode::Best).await? {
        RateLimitResult::Allowed(authorization) => authorization,
        RateLimitResult::RateLimited(authorization, retry_at) => {
            return Err(Web3ProxyError::RateLimited(authorization, retry_at));
        }
        // TODO: don't panic. give the user an error
        x => unimplemented!("rate_limit_login shouldn't ever see these: {:?}", x),
    };

    Ok(authorization)
}

/// semaphore won't ever be None, but its easier if key auth and ip auth work the same way
/// keep the semaphore alive until the user's request is entirely complete
pub async fn ip_is_authorized(
    app: &Arc<App>,
    ip: &IpAddr,
    origin: Option<&Origin>,
    proxy_mode: ProxyMode,
) -> Web3ProxyResult<Authorization> {
    // TODO: i think we could write an `impl From` for this
    // TODO: move this to an AuthorizedUser extrator
    let authorization = match app.rate_limit_public(ip, origin, proxy_mode).await? {
        RateLimitResult::Allowed(authorization) => authorization,
        RateLimitResult::RateLimited(authorization, retry_at) => {
            // TODO: in the background, emit a stat (maybe simplest to use a channel?)
            return Err(Web3ProxyError::RateLimited(authorization, retry_at));
        }
        // TODO: don't panic. give the user an error
        x => unimplemented!("rate_limit_by_ip shouldn't ever see these: {:?}", x),
    };

    // in the background, add the hashed ip to a recent_users map
    if app.config.public_recent_ips_salt.is_some() {
        let app = app.clone();
        let ip = *ip;

        let f = async move {
            let now = Utc::now().timestamp();

            if let Ok(mut redis_conn) = app.redis_conn().await {
                let salt = app
                    .config
                    .public_recent_ips_salt
                    .as_ref()
                    .expect("public_recent_ips_salt must exist in here");

                let salted_ip = format!("{}:{}", salt, ip);

                let hashed_ip = Bytes::from(keccak256(salted_ip.as_bytes()));

                let recent_ip_key = format!("recent_users:ip:{}", app.config.chain_id);

                redis_conn
                    .zadd(recent_ip_key, hashed_ip.to_string(), now)
                    .await?;
            };

            Ok::<_, Web3ProxyError>(())
        }
        .map_err(|err| {
            warn!(?err, "background update of recent_users:ip failed");

            err
        });

        tokio::spawn(f);
    }

    Ok(authorization)
}

/// like app.rate_limit_by_rpc_key but converts to a Web3ProxyError;
/// keep the semaphore alive until the user's request is entirely complete
pub async fn key_is_authorized(
    app: &Arc<App>,
    rpc_key: &RpcSecretKey,
    ip: &IpAddr,
    origin: Option<&Origin>,
    proxy_mode: ProxyMode,
    referer: Option<&Referer>,
    user_agent: Option<&UserAgent>,
) -> Web3ProxyResult<Authorization> {
    // check the rate limits. error if over the limit
    // TODO: i think this should be in an "impl From" or "impl Into"
    let authorization = match app
        .rate_limit_premium(ip, origin, proxy_mode, referer, rpc_key, user_agent)
        .await?
    {
        RateLimitResult::Allowed(authorization) => authorization,
        RateLimitResult::RateLimited(authorization, retry_at) => {
            return Err(Web3ProxyError::RateLimited(authorization, retry_at));
        }
        RateLimitResult::UnknownKey => return Err(Web3ProxyError::UnknownKey),
    };

    // TODO: DRY and maybe optimize the hashing
    // in the background, add the ip to a recent_users map
    if app.config.public_recent_ips_salt.is_some() {
        let app = app.clone();
        let user_id = authorization.checks.user_id;
        let f = async move {
            let now = Utc::now().timestamp();

            if let Ok(mut redis_conn) = app.redis_conn().await {
                let salt = app
                    .config
                    .public_recent_ips_salt
                    .as_ref()
                    .expect("public_recent_ips_salt must exist in here");

                let salted_user_id = format!("{}:{}", salt, user_id);

                let hashed_user_id = Bytes::from(keccak256(salted_user_id.as_bytes()));

                let recent_user_id_key = format!("recent_users:id:{}", app.config.chain_id);

                redis_conn
                    .zadd(recent_user_id_key, hashed_user_id.to_string(), now)
                    .await?;
            }

            Ok::<_, Web3ProxyError>(())
        }
        .map_err(|err| {
            warn!(?err, "background update of recent_users:id failed");

            err
        });

        tokio::spawn(f);
    }

    Ok(authorization)
}

impl App {
    /// Limit the number of concurrent requests from the given ip address.
    /// TODO: should this take an Authorization isntead of an IpAddr?
    pub async fn permit_public_concurrency(
        &self,
        ip: &IpAddr,
    ) -> Web3ProxyResult<Option<OwnedSemaphorePermit>> {
        if let Some(max_concurrent_requests) = self.config.public_max_concurrent_requests {
            let semaphore = self
                .ip_semaphores
                .get_with_by_ref(ip, async {
                    // TODO: set max_concurrent_requests dynamically based on load?
                    let s = Semaphore::new(max_concurrent_requests);
                    Arc::new(s)
                })
                .await;

            let semaphore_permit = tokio::select! {
                biased;

                p = semaphore.acquire_owned() => {
                    p
                }
                p = self.bonus_ip_concurrency.clone().acquire_owned() => {
                    p
                }
            }?;

            Ok(Some(semaphore_permit))
        } else {
            Ok(None)
        }
    }

    /// Limit the number of concurrent requests for a given user across all of their keys
    /// keep the semaphore alive until the user's request is entirely complete
    pub async fn permit_premium_concurrency(
        &self,
        authorization: &Authorization,
    ) -> Web3ProxyResult<Option<OwnedSemaphorePermit>> {
        let authorization_checks = &authorization.checks;

        if let Some(max_concurrent_requests) = authorization_checks.max_concurrent_requests {
            let user_id = authorization_checks
                .user_id
                .try_into()
                .or(Err(Web3ProxyError::UserIdZero))?;

            let semaphore = self
                .user_semaphores
                .get_with_by_ref(&(user_id, authorization.ip), async move {
                    let s = Semaphore::new(max_concurrent_requests as usize);
                    Arc::new(s)
                })
                .await;

            let semaphore_permit = tokio::select! {
                biased;

                p = semaphore.acquire_owned() => {
                    p
                }
                p = self.bonus_user_concurrency.clone().acquire_owned() => {
                    p
                }
                p = self.bonus_ip_concurrency.clone().acquire_owned() => {
                    p
                }
            }?;

            Ok(Some(semaphore_permit))
        } else {
            // unlimited concurrency
            Ok(None)
        }
    }

    /// Verify that the given bearer token and address are allowed to take the specified action.
    /// This includes concurrent request limiting.
    /// keep the semaphore alive until the user's request is entirely complete
    pub async fn bearer_is_authorized(&self, bearer: Bearer) -> Web3ProxyResult<user::Model> {
        // get the user id for this bearer token
        let user_bearer_token = UserBearerToken::try_from(bearer)?;

        // get the attached address from the database for the given auth_token.
        let db_replica = global_db_replica_conn()?;

        let user_bearer_uuid: Uuid = user_bearer_token.into();

        let user = user::Entity::find()
            .left_join(login::Entity)
            .filter(login::Column::BearerToken.eq(user_bearer_uuid))
            .one(db_replica.as_ref())
            .await
            .web3_context("fetching user from db by bearer token")?
            .web3_context("unknown bearer token")?;

        Ok(user)
    }

    pub async fn rate_limit_login(
        &self,
        ip: IpAddr,
        proxy_mode: ProxyMode,
    ) -> Web3ProxyResult<RateLimitResult> {
        // TODO: if ip is on the local network, always allow?

        // we don't care about user agent or origin or referer
        let authorization = Authorization::external(
            &self.config.allowed_origin_requests_per_period,
            &ip,
            None,
            proxy_mode,
            None,
            None,
        )?;

        let label = ip.to_string();

        redis_rate_limit(
            &self.login_rate_limiter,
            authorization,
            None,
            Some(&label),
            None,
        )
        .await
    }

    /// origin is included because it can override the default rate limits
    pub async fn rate_limit_public(
        &self,
        ip: &IpAddr,
        origin: Option<&Origin>,
        proxy_mode: ProxyMode,
    ) -> Web3ProxyResult<RateLimitResult> {
        if ip.is_loopback() {
            // TODO: localhost being unlimited should be optional
            let authorization = Authorization::internal()?;

            return Ok(RateLimitResult::Allowed(authorization));
        }

        let allowed_origin_requests_per_period = &self.config.allowed_origin_requests_per_period;

        // ip rate limits don't check referer or user agent
        // they do check origin because we can override rate limits for some origins
        let authorization = Authorization::external(
            allowed_origin_requests_per_period,
            ip,
            origin,
            proxy_mode,
            None,
            None,
        )?;

        if let Some(rate_limiter) = &self.frontend_public_rate_limiter {
            let mut x = deferred_redis_rate_limit(authorization, *ip, None, rate_limiter).await?;

            if let RateLimitResult::RateLimited(authorization, retry_at) = x {
                // we got rate limited, try bonus_frontend_public_rate_limiter
                x = redis_rate_limit(
                    &self.bonus_frontend_public_rate_limiter,
                    authorization,
                    retry_at,
                    None,
                    None,
                )
                .await?;
            }

            if let RateLimitResult::Allowed(a) = x {
                x = RateLimitResult::Allowed(a)
            }

            debug_assert!(!matches!(x, RateLimitResult::UnknownKey));

            Ok(x)
        } else {
            // TODO: if no redis, rate limit with a local cache? "warn!" probably isn't right
            Ok(RateLimitResult::Allowed(authorization))
        }
    }

    // check the local cache for user data, or query the database
    pub(crate) async fn authorization_checks(
        &self,
        proxy_mode: ProxyMode,
        rpc_secret_key: &RpcSecretKey,
    ) -> Web3ProxyResult<AuthorizationChecks> {
        // TODO: move onto a helper function

        let x = self
            .rpc_secret_key_cache
            .try_get_with_by_ref(rpc_secret_key, async move {
                let db_replica = global_db_replica_conn()?;

                // TODO: join the user table to this to return the User? we don't always need it
                // TODO: join on secondary users
                // TODO: join on user tier
                match rpc_key::Entity::find()
                    .filter(rpc_key::Column::SecretKey.eq(<Uuid>::from(*rpc_secret_key)))
                    .filter(rpc_key::Column::Active.eq(true))
                    .one(db_replica.as_ref())
                    .await?
                {
                    Some(rpc_key_model) => {
                        // TODO: move these splits into helper functions
                        // TODO: can we have sea orm handle this for us?
                        let allowed_ips: Option<Vec<IpNet>> =
                            if let Some(allowed_ips) = rpc_key_model.allowed_ips {
                                let x = allowed_ips
                                    .split(',')
                                    .map(|x| x.trim().parse::<IpNet>())
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
                                    .map(|x| HeaderValue::from_str(x.trim()))
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
                                    .map(|x| {
                                        x.trim()
                                            .parse::<Referer>()
                                            .or(Err(Web3ProxyError::InvalidReferer))
                                    })
                                    .collect::<Result<Vec<_>, _>>()?;

                                Some(x)
                            } else {
                                None
                            };

                        let allowed_user_agents: Option<Vec<UserAgent>> =
                            if let Some(allowed_user_agents) = rpc_key_model.allowed_user_agents {
                                let x: Result<Vec<_>, _> = allowed_user_agents
                                    .split(',')
                                    .map(|x| {
                                        x.trim()
                                            .parse::<UserAgent>()
                                            .or(Err(Web3ProxyError::InvalidUserAgent))
                                    })
                                    .collect();

                                Some(x?)
                            } else {
                                None
                            };

                        // Get the user_tier
                        let user_model = user::Entity::find_by_id(rpc_key_model.user_id)
                            .one(db_replica.as_ref())
                            .await?
                            .web3_context(
                                "user model was not found, but every rpc_key should have a user",
                            )?;

                        let mut user_tier_model = user_tier::Entity::find_by_id(
                            user_model.user_tier_id,
                        )
                        .one(db_replica.as_ref())
                        .await?
                        .web3_context(
                            "related user tier not found, but every user should have a tier",
                        )?;

                        let latest_balance = self
                            .user_balance_cache
                            .get_or_insert(db_replica.as_ref(), rpc_key_model.user_id)
                            .await?;

                        let paid_credits_used: bool;
                        if let Some(downgrade_user_tier) = user_tier_model.downgrade_tier_id {
                            trace!("user belongs to a premium tier. checking balance");

                            let active_premium = latest_balance.read().await.active_premium();

                            // only consider the user premium if they have paid at least $10 and have a balance > $.01
                            // otherwise, set user_tier_model to the downograded tier
                            if active_premium {
                                paid_credits_used = true;
                            } else {
                                paid_credits_used = false;

                                // TODO: include boolean to mark that the user is downgraded
                                user_tier_model =
                                    user_tier::Entity::find_by_id(downgrade_user_tier)
                                        .one(db_replica.as_ref())
                                        .await?
                                        .web3_context(format!(
                                            "downgrade user tier ({}) is missing!",
                                            downgrade_user_tier
                                        ))?;
                            }
                        } else {
                            paid_credits_used = false;
                        }

                        let rpc_key_id =
                            Some(rpc_key_model.id.try_into().context("db ids are never 0")?);

                        Ok::<_, Web3ProxyError>(AuthorizationChecks {
                            allowed_ips,
                            allowed_origins,
                            allowed_referers,
                            allowed_user_agents,
                            latest_balance,
                            // TODO: is floating point math going to scale this correctly?
                            log_revert_chance: (rpc_key_model.log_revert_chance * u16::MAX as f64)
                                as u16,
                            max_concurrent_requests: user_tier_model.max_concurrent_requests,
                            max_requests_per_period: user_tier_model.max_requests_per_period,
                            private_txs: rpc_key_model.private_txs,
                            proxy_mode,
                            rpc_secret_key: Some(*rpc_secret_key),
                            rpc_secret_key_id: rpc_key_id,
                            user_id: rpc_key_model.user_id,
                            paid_credits_used,
                        })
                    }
                    None => Ok(AuthorizationChecks::default()),
                }
            })
            .await?;

        Ok(x)
    }

    /// Authorize the key/ip/origin/referer/useragent and handle rate and concurrency limits
    pub async fn rate_limit_premium(
        &self,
        ip: &IpAddr,
        origin: Option<&Origin>,
        proxy_mode: ProxyMode,
        referer: Option<&Referer>,
        rpc_key: &RpcSecretKey,
        user_agent: Option<&UserAgent>,
    ) -> Web3ProxyResult<RateLimitResult> {
        let authorization_checks = match self.authorization_checks(proxy_mode, rpc_key).await {
            Ok(x) => x,
            Err(err) => {
                if let Ok(_err) = err.split_db_errors() {
                    // // TODO: this is too verbose during an outage. the warnings on the config reloader should be fine
                    // warn!(
                    //     ?err,
                    //     "db is down. cannot check rpc key. fallback to ip rate limits"
                    // );
                    return self.rate_limit_public(ip, origin, proxy_mode).await;
                }

                return Err(err);
            }
        };

        // if no rpc_key_id matching the given rpc was found, then we can't rate limit by key
        if authorization_checks.rpc_secret_key_id.is_none() {
            trace!("unknown key. falling back to free limits");
            return self.rate_limit_public(ip, origin, proxy_mode).await;
        }

        let authorization = Authorization::try_new(
            authorization_checks,
            ip,
            origin,
            referer,
            user_agent,
            AuthorizationType::Frontend,
        )?;

        // user key is valid. now check rate limits
        if let Some(user_max_requests_per_period) = authorization.checks.max_requests_per_period {
            if let Some(rate_limiter) = &self.frontend_premium_rate_limiter {
                let key = RegisteredUserRateLimitKey(authorization.checks.user_id, *ip);

                let mut x = deferred_redis_rate_limit(
                    authorization,
                    key,
                    Some(user_max_requests_per_period),
                    rate_limiter,
                )
                .await?;

                if let RateLimitResult::RateLimited(authorization, retry_at) = x {
                    // rate limited by the user's key+ip. check to see if there are any limits available in the bonus premium pool
                    x = redis_rate_limit(
                        &self.bonus_frontend_premium_rate_limiter,
                        authorization,
                        retry_at,
                        None,
                        None,
                    )
                    .await?;
                }

                if let RateLimitResult::RateLimited(authorization, retry_at) = x {
                    // premium got rate limited too. check the bonus public pool
                    x = redis_rate_limit(
                        &self.bonus_frontend_public_rate_limiter,
                        authorization,
                        retry_at,
                        None,
                        None,
                    )
                    .await?;
                }

                if let RateLimitResult::Allowed(a) = x {
                    // only allow this rpc_key to run a limited amount of concurrent requests
                    x = RateLimitResult::Allowed(a)
                }

                debug_assert!(!matches!(x, RateLimitResult::UnknownKey));

                return Ok(x);
            } else {
                // TODO: if no redis, rate limit with just a local cache?
            }
        }

        Ok(RateLimitResult::Allowed(authorization))
    }
}

impl Authorization {
    pub async fn check_again(
        &self,
        app: &Arc<App>,
    ) -> Web3ProxyResult<(Arc<Self>, Option<OwnedSemaphorePermit>)> {
        // TODO: we could probably do this without clones. but this is easy
        let (a, p) = if let Some(ref rpc_secret_key) = self.checks.rpc_secret_key {
            let a = key_is_authorized(
                app,
                rpc_secret_key,
                &self.ip,
                self.origin.as_ref(),
                self.checks.proxy_mode,
                self.referer.as_ref(),
                self.user_agent.as_ref(),
            )
            .await?;

            let p = app.permit_premium_concurrency(&a).await?;

            (a, p)
        } else {
            let a = ip_is_authorized(app, &self.ip, self.origin.as_ref(), self.checks.proxy_mode)
                .await?;

            let p = app.permit_public_concurrency(&self.ip).await?;

            (a, p)
        };

        let a = Arc::new(a);

        Ok((a, p))
    }
}

/// this fails open!
/// this never includes a semaphore! if you want one, add it after this call
/// if `max_requests_per_period` is none, the limit in the authorization is used
pub async fn deferred_redis_rate_limit<K>(
    authorization: Authorization,
    key: K,
    max_requests_per_period: Option<u64>,
    rate_limiter: &DeferredRateLimiter<K>,
) -> Web3ProxyResult<RateLimitResult>
where
    K: Send + Sync + Copy + Clone + Display + Hash + Eq + PartialEq + 'static,
{
    let max_requests_per_period =
        max_requests_per_period.or(authorization.checks.max_requests_per_period);

    let x = match rate_limiter.throttle(key, max_requests_per_period, 1).await {
        Ok(DeferredRateLimitResult::Allowed) => RateLimitResult::Allowed(authorization),
        Ok(DeferredRateLimitResult::RetryAt(retry_at)) => {
            // TODO: set headers so they know when they can retry
            // TODO: debug or trace?
            // this is too verbose, but a stat might be good
            // TODO: emit a stat
            // trace!(?rpc_key, "rate limit exceeded until {:?}", retry_at);
            RateLimitResult::RateLimited(authorization, Some(retry_at))
        }
        Ok(DeferredRateLimitResult::RetryNever) => {
            // TODO: keys are secret. don't log them!
            // trace!(?rpc_key, "rate limit is 0");
            // TODO: emit a stat
            RateLimitResult::RateLimited(authorization, None)
        }
        Err(err) => {
            // internal error, not rate limit being hit
            error!(?err, %key, "rate limiter is unhappy. allowing key");

            RateLimitResult::Allowed(authorization)
        }
    };

    Ok(x)
}

/// this never includes a semaphore! if you want one, add it after this call
/// if `max_requests_per_period` is none, the limit in the authorization is used
pub async fn redis_rate_limit(
    rate_limiter: &Option<RedisRateLimiter>,
    authorization: Authorization,
    mut retry_at: Option<Instant>,
    label: Option<&str>,
    max_requests_per_period: Option<u64>,
) -> Web3ProxyResult<RateLimitResult> {
    let max_requests_per_period =
        max_requests_per_period.or(authorization.checks.max_requests_per_period);

    let x = if let Some(rate_limiter) = rate_limiter {
        match rate_limiter
            .throttle_label(label.unwrap_or_default(), max_requests_per_period, 1)
            .await
        {
            Ok(RedisRateLimitResult::Allowed(..)) => RateLimitResult::Allowed(authorization),
            Ok(RedisRateLimitResult::RetryAt(new_retry_at, ..)) => {
                retry_at = retry_at.min(Some(new_retry_at));

                RateLimitResult::RateLimited(authorization, retry_at)
            }
            Ok(RedisRateLimitResult::RetryNever) => {
                RateLimitResult::RateLimited(authorization, retry_at)
            }
            Err(err) => {
                // this an internal error of some kind, not the rate limit being hit
                error!("rate limiter is unhappy. allowing ip. err={:?}", err);

                RateLimitResult::Allowed(authorization)
            }
        }
    } else {
        RateLimitResult::Allowed(authorization)
    };

    Ok(x)
}
