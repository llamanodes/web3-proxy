use super::errors::FrontendErrorResponse;
use crate::app::{UserKeyData, Web3ProxyApp};
use anyhow::Context;
use axum::headers::{Origin, Referer, UserAgent};
use deferred_rate_limiter::DeferredRateLimitResult;
use entities::user_keys;
use ipnet::IpNet;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde::Serialize;
use std::{net::IpAddr, sync::Arc};
use tokio::time::Instant;
use tracing::{error, trace};
use uuid::Uuid;

#[derive(Debug)]
pub enum RateLimitResult {
    /// contains the IP of the anonymous user
    AllowedIp(IpAddr),
    /// contains the user_key_id of an authenticated user
    AllowedUser(UserKeyData),
    /// contains the IP and retry_at of the anonymous user
    RateLimitedIp(IpAddr, Option<Instant>),
    /// contains the user_key_id and retry_at of an authenticated user key
    RateLimitedUser(UserKeyData, Option<Instant>),
    /// This key is not in our database. Deny access!
    UnknownKey,
}

#[derive(Debug, Serialize)]
pub struct AuthorizedKey {
    ip: IpAddr,
    origin: Option<String>,
    user_key_id: u64,
    // TODO: what else?
}

impl AuthorizedKey {
    pub fn try_new(
        ip: IpAddr,
        origin: Option<Origin>,
        referer: Option<Referer>,
        user_agent: Option<UserAgent>,
        user_data: UserKeyData,
    ) -> anyhow::Result<Self> {
        // check ip
        match &user_data.allowed_ips {
            None => {}
            Some(allowed_ips) => {
                if !allowed_ips.iter().any(|x| x.contains(&ip)) {
                    return Err(anyhow::anyhow!("IP is not allowed!"));
                }
            }
        }

        // check origin
        // TODO: do this with the Origin type instead of a String?
        let origin = origin.map(|x| x.to_string());
        match (&origin, &user_data.allowed_origins) {
            (None, None) => {}
            (Some(_), None) => {}
            (None, Some(_)) => return Err(anyhow::anyhow!("Origin required")),
            (Some(origin), Some(allowed_origins)) => {
                let origin = origin.to_string();

                if !allowed_origins.contains(&origin) {
                    return Err(anyhow::anyhow!("IP is not allowed!"));
                }
            }
        }

        // check referer
        match (referer, &user_data.allowed_referers) {
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
        match (user_agent, &user_data.allowed_user_agents) {
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
            user_key_id: user_data.user_key_id,
        })
    }
}

#[derive(Debug, Serialize)]
pub enum AuthorizedRequest {
    /// Request from the app itself
    Internal(#[serde(skip)] Option<DatabaseConnection>),
    /// Request from an anonymous IP address
    Ip(#[serde(skip)] Option<DatabaseConnection>, IpAddr),
    /// Request from an authenticated and authorized user
    User(#[serde(skip)] Option<DatabaseConnection>, AuthorizedKey),
}

impl AuthorizedRequest {
    pub fn db_conn(&self) -> Option<&DatabaseConnection> {
        match self {
            Self::Internal(x) => x.as_ref(),
            Self::Ip(x, _) => x.as_ref(),
            Self::User(x, _) => x.as_ref(),
        }
    }
}

pub async fn ip_is_authorized(
    app: &Web3ProxyApp,
    ip: IpAddr,
) -> Result<AuthorizedRequest, FrontendErrorResponse> {
    // TODO: i think we could write an `impl From` for this
    let ip = match app.rate_limit_by_ip(ip).await? {
        RateLimitResult::AllowedIp(x) => x,
        RateLimitResult::RateLimitedIp(x, retry_at) => {
            return Err(FrontendErrorResponse::RateLimitedIp(x, retry_at));
        }
        // TODO: don't panic. give the user an error
        x => unimplemented!("rate_limit_by_ip shouldn't ever see these: {:?}", x),
    };

    let db = app.db_conn.clone();

    Ok(AuthorizedRequest::Ip(db, ip))
}

pub async fn key_is_authorized(
    app: &Web3ProxyApp,
    user_key: Uuid,
    ip: IpAddr,
    origin: Option<Origin>,
    referer: Option<Referer>,
    user_agent: Option<UserAgent>,
) -> Result<AuthorizedRequest, FrontendErrorResponse> {
    // check the rate limits. error if over the limit
    let user_data = match app.rate_limit_by_key(user_key).await? {
        RateLimitResult::AllowedUser(x) => x,
        RateLimitResult::RateLimitedUser(x, retry_at) => {
            return Err(FrontendErrorResponse::RateLimitedUser(x, retry_at));
        }
        RateLimitResult::UnknownKey => return Err(FrontendErrorResponse::UnknownKey),
        // TODO: don't panic. give the user an error
        x => unimplemented!("rate_limit_by_key shouldn't ever see these: {:?}", x),
    };

    let authorized_user = AuthorizedKey::try_new(ip, origin, referer, user_agent, user_data)?;

    let db = app.db_conn.clone();

    Ok(AuthorizedRequest::User(db, authorized_user))
}

impl Web3ProxyApp {
    pub async fn rate_limit_by_ip(&self, ip: IpAddr) -> anyhow::Result<RateLimitResult> {
        // TODO: dry this up with rate_limit_by_key
        // TODO: have a local cache because if we hit redis too hard we get errors
        // TODO: query redis in the background so that users don't have to wait on this network request
        if let Some(rate_limiter) = &self.frontend_ip_rate_limiter {
            match rate_limiter.throttle(ip, None, 1).await {
                Ok(DeferredRateLimitResult::Allowed) => Ok(RateLimitResult::AllowedIp(ip)),
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
                    Ok(RateLimitResult::AllowedIp(ip))
                }
            }
        } else {
            // TODO: if no redis, rate limit with a local cache? "warn!" probably isn't right
            todo!("no rate limiter");
        }
    }

    // check the local cache for user data, or query the database
    pub(crate) async fn user_data(&self, user_key: Uuid) -> anyhow::Result<UserKeyData> {
        let user_data: Result<_, Arc<anyhow::Error>> = self
            .user_cache
            .try_get_with(user_key, async move {
                trace!(?user_key, "user_cache miss");

                let db = self.db_conn.as_ref().context("no database")?;

                // TODO: join the user table to this to return the User? we don't always need it
                match user_keys::Entity::find()
                    .filter(user_keys::Column::ApiKey.eq(user_key))
                    .filter(user_keys::Column::Active.eq(true))
                    .one(db)
                    .await?
                {
                    Some(user_key_model) => {
                        let allowed_ips: Option<Vec<IpNet>> =
                            user_key_model.allowed_ips.map(|allowed_ips| {
                                serde_json::from_str::<Vec<String>>(&allowed_ips)
                                    .expect("allowed_ips should always parse")
                                    .into_iter()
                                    // TODO: try_for_each
                                    .map(|x| {
                                        x.parse::<IpNet>().expect("ip address should always parse")
                                    })
                                    .collect()
                            });

                        // TODO: should this be an Option<Vec<Origin>>?
                        let allowed_origins =
                            user_key_model.allowed_origins.map(|allowed_origins| {
                                serde_json::from_str::<Vec<String>>(&allowed_origins)
                                    .expect("allowed_origins should always parse")
                            });

                        let allowed_referers =
                            user_key_model.allowed_referers.map(|allowed_referers| {
                                serde_json::from_str::<Vec<String>>(&allowed_referers)
                                    .expect("allowed_referers should always parse")
                                    .into_iter()
                                    // TODO: try_for_each
                                    .map(|x| {
                                        x.parse::<Referer>().expect("referer should always parse")
                                    })
                                    .collect()
                            });

                        let allowed_user_agents =
                            user_key_model
                                .allowed_user_agents
                                .map(|allowed_user_agents| {
                                    serde_json::from_str::<Vec<String>>(&allowed_user_agents)
                                        .expect("allowed_user_agents should always parse")
                                        .into_iter()
                                        // TODO: try_for_each
                                        .map(|x| {
                                            x.parse::<UserAgent>()
                                                .expect("user agent should always parse")
                                        })
                                        .collect()
                                });

                        Ok(UserKeyData {
                            user_key_id: user_key_model.id,
                            user_max_requests_per_period: user_key_model.requests_per_minute,
                            allowed_ips,
                            allowed_origins,
                            allowed_referers,
                            allowed_user_agents,
                        })
                    }
                    None => Ok(UserKeyData {
                        user_key_id: 0,
                        user_max_requests_per_period: Some(0),
                        allowed_ips: None,
                        allowed_origins: None,
                        allowed_referers: None,
                        allowed_user_agents: None,
                    }),
                }
            })
            .await;

        // TODO: i'm not actually sure about this expect
        user_data.map_err(|err| Arc::try_unwrap(err).expect("this should be the only reference"))
    }

    pub async fn rate_limit_by_key(&self, user_key: Uuid) -> anyhow::Result<RateLimitResult> {
        let user_data = self.user_data(user_key).await?;

        if user_data.user_key_id == 0 {
            return Ok(RateLimitResult::UnknownKey);
        }

        let user_max_requests_per_period = match user_data.user_max_requests_per_period {
            None => return Ok(RateLimitResult::AllowedUser(user_data)),
            Some(x) => x,
        };

        // user key is valid. now check rate limits
        if let Some(rate_limiter) = &self.frontend_key_rate_limiter {
            match rate_limiter
                .throttle(user_key, Some(user_max_requests_per_period), 1)
                .await
            {
                Ok(DeferredRateLimitResult::Allowed) => Ok(RateLimitResult::AllowedUser(user_data)),
                Ok(DeferredRateLimitResult::RetryAt(retry_at)) => {
                    // TODO: set headers so they know when they can retry
                    // TODO: debug or trace?
                    // this is too verbose, but a stat might be good
                    // TODO: keys are secrets! use the id instead
                    trace!(?user_key, "rate limit exceeded until {:?}", retry_at);
                    Ok(RateLimitResult::RateLimitedUser(user_data, Some(retry_at)))
                }
                Ok(DeferredRateLimitResult::RetryNever) => {
                    // TODO: keys are secret. don't log them!
                    trace!(?user_key, "rate limit is 0");
                    Ok(RateLimitResult::RateLimitedUser(user_data, None))
                }
                Err(err) => {
                    // internal error, not rate limit being hit
                    // TODO: i really want axum to do this for us in a single place.
                    error!(?err, "rate limiter is unhappy. allowing ip");
                    Ok(RateLimitResult::AllowedUser(user_data))
                }
            }
        } else {
            // TODO: if no redis, rate limit with just a local cache?
            // if we don't have redis, we probably don't have a db, so this probably will never happen
            Err(anyhow::anyhow!("no redis. cannot rate limit"))
        }
    }
}
