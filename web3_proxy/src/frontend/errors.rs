//! Utlities for logging errors for admins and displaying errors to users.

use super::authorization::Authorization;
use crate::jsonrpc::JsonRpcForwardedResponse;

use std::net::IpAddr;
use std::sync::Arc;

use axum::{
    headers,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use derive_more::{Display, Error, From};
use http::header::InvalidHeaderValue;
use ipnet::AddrParseError;
use log::{debug, error, info, trace, warn};
use migration::sea_orm::DbErr;
use redis_rate_limiter::redis::RedisError;
use reqwest::header::ToStrError;
use tokio::{sync::AcquireError, task::JoinError, time::Instant};

pub type Web3ProxyResult<T> = Result<T, Web3ProxyError>;
// TODO: take "IntoResponse" instead of Response?
pub type Web3ProxyResponse = Web3ProxyResult<Response>;

// TODO:
#[derive(Debug, Display, Error, From)]
pub enum Web3ProxyError {
    AccessDenied,
    #[error(ignore)]
    Anyhow(anyhow::Error),
    Arc(Arc<Web3ProxyError>),
    #[error(ignore)]
    #[from(ignore)]
    BadRequest(String),
    BadRouting,
    Database(DbErr),
    #[display(fmt = "{:#?}, {:#?}", _0, _1)]
    EipVerificationFailed(Box<Web3ProxyError>, Box<Web3ProxyError>),
    EthersHttpClientError(ethers::prelude::HttpClientError),
    EthersProviderError(ethers::prelude::ProviderError),
    EthersWsClientError(ethers::prelude::WsClientError),
    FlumeRecvError(flume::RecvError),
    GasEstimateNotU256,
    Headers(headers::Error),
    HeaderToString(ToStrError),
    InfluxDb2RequestError(influxdb2::RequestError),
    #[display(fmt = "{} > {}", min, max)]
    #[from(ignore)]
    InvalidBlockBounds {
        min: u64,
        max: u64,
    },
    InvalidHeaderValue(InvalidHeaderValue),
    InvalidEip,
    InvalidInviteCode,
    InvalidReferralCode,
    InvalidReferer,
    InvalidSignatureLength,
    InvalidUserAgent,
    InvalidUserKey,
    IpAddrParse(AddrParseError),
    #[error(ignore)]
    #[from(ignore)]
    IpNotAllowed(IpAddr),
    JoinError(JoinError),
    #[display(fmt = "{:?}", _0)]
    #[error(ignore)]
    JsonRpcForwardedError(JsonRpcForwardedResponse),
    #[display(fmt = "{:?}", _0)]
    #[error(ignore)]
    MsgPackEncode(rmp_serde::encode::Error),
    NoBlockNumberOrHash,
    NoBlocksKnown,
    NoConsensusHeadBlock,
    NoHandleReady,
    NoServersSynced,
    #[display(fmt = "{}/{}", num_known, min_head_rpcs)]
    #[from(ignore)]
    NotEnoughRpcs {
        num_known: usize,
        min_head_rpcs: usize,
    },
    NotFound,
    NotImplemented,
    OriginRequired,
    #[error(ignore)]
    #[from(ignore)]
    OriginNotAllowed(headers::Origin),
    #[display(fmt = "{:?}", _0)]
    #[error(ignore)]
    ParseBytesError(Option<ethers::types::ParseBytesError>),
    ParseMsgError(siwe::ParseError),
    ParseAddressError,
    #[display(fmt = "{:?}, {:?}", _0, _1)]
    RateLimited(Authorization, Option<Instant>),
    Redis(RedisError),
    RefererRequired,
    #[display(fmt = "{:?}", _0)]
    #[error(ignore)]
    #[from(ignore)]
    RefererNotAllowed(headers::Referer),
    SemaphoreAcquireError(AcquireError),
    SendAppStatError(flume::SendError<crate::stats::AppStat>),
    SerdeJson(serde_json::Error),
    /// simple way to return an error message to the user and an anyhow to our logs
    #[display(fmt = "{}, {}, {:?}", _0, _1, _2)]
    StatusCode(StatusCode, String, Option<anyhow::Error>),
    /// TODO: what should be attached to the timout?
    #[display(fmt = "{:?}", _0)]
    #[error(ignore)]
    Timeout(Option<tokio::time::error::Elapsed>),
    UlidDecode(ulid::DecodeError),
    UnknownBlockNumber,
    UnknownKey,
    UserAgentRequired,
    #[error(ignore)]
    UserAgentNotAllowed(headers::UserAgent),
    UserIdZero,
    VerificationError(siwe::VerificationError),
    WatchRecvError(tokio::sync::watch::error::RecvError),
    WatchSendError,
    WebsocketOnly,
    #[display(fmt = "{:?}, {}", _0, _1)]
    #[error(ignore)]
    WithContext(Option<Box<Web3ProxyError>>, String),
}

impl Web3ProxyError {
    pub fn into_response_parts(self) -> (StatusCode, JsonRpcForwardedResponse) {
        match self {
            Self::AccessDenied => {
                // TODO: attach something to this trace. probably don't include much in the message though. don't want to leak creds by accident
                trace!("access denied");
                (
                    StatusCode::FORBIDDEN,
                    JsonRpcForwardedResponse::from_string(
                        // TODO: is it safe to expose all of our anyhow strings?
                        "FORBIDDEN".to_string(),
                        Some(StatusCode::FORBIDDEN.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::Anyhow(err) => {
                warn!("anyhow. err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcForwardedResponse::from_string(
                        // TODO: is it safe to expose all of our anyhow strings?
                        err.to_string(),
                        Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::BadRequest(err) => {
                debug!("BAD_REQUEST: {}", err);
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        &format!("bad request: {}", err),
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::BadRouting => {
                error!("BadRouting");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcForwardedResponse::from_str(
                        "bad routing",
                        Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::Database(err) => {
                error!("database err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcForwardedResponse::from_str(
                        "database error!",
                        Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::EipVerificationFailed(err_1, err_191) => {
                info!(
                    "EipVerificationFailed err_1={:#?} err2={:#?}",
                    err_1, err_191
                );
                (
                    StatusCode::UNAUTHORIZED,
                    JsonRpcForwardedResponse::from_string(
                        format!(
                            "both the primary and eip191 verification failed: {:#?}; {:#?}",
                            err_1, err_191
                        ),
                        Some(StatusCode::UNAUTHORIZED.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::EthersHttpClientError(err) => {
                warn!("EthersHttpClientError err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcForwardedResponse::from_str(
                        "ether http client error",
                        Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::EthersProviderError(err) => {
                warn!("EthersProviderError err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcForwardedResponse::from_str(
                        "ether provider error",
                        Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::EthersWsClientError(err) => {
                warn!("EthersWsClientError err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcForwardedResponse::from_str(
                        "ether ws client error",
                        Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::FlumeRecvError(err) => {
                warn!("FlumeRecvError err={:#?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcForwardedResponse::from_str(
                        "flume recv error!",
                        Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::JsonRpcForwardedError(x) => (StatusCode::OK, x),
            Self::GasEstimateNotU256 => {
                warn!("GasEstimateNotU256");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcForwardedResponse::from_str(
                        "gas estimate result is not an U256",
                        Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::Headers(err) => {
                warn!("HeadersError {:?}", err);
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        &format!("{}", err),
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::InfluxDb2RequestError(err) => {
                // TODO: attach a request id to the message and to this error so that if people report problems, we can dig in sentry to find out more
                error!("influxdb2 err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcForwardedResponse::from_str(
                        "influxdb2 error!",
                        Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::InvalidBlockBounds { min, max } => {
                debug!("InvalidBlockBounds min={} max={}", min, max);
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_string(
                        format!(
                            "Invalid blocks bounds requested. min ({}) > max ({})",
                            min, max
                        ),
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::IpAddrParse(err) => {
                warn!("IpAddrParse err={:?}", err);
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        &format!("{}", err),
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::IpNotAllowed(ip) => {
                warn!("IpNotAllowed ip={})", ip);
                (
                    StatusCode::FORBIDDEN,
                    JsonRpcForwardedResponse::from_string(
                        format!("IP ({}) is not allowed!", ip),
                        Some(StatusCode::FORBIDDEN.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::InvalidHeaderValue(err) => {
                warn!("InvalidHeaderValue err={:?}", err);
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        &format!("{}", err),
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::InvalidEip => {
                warn!("InvalidEip");
                (
                    StatusCode::UNAUTHORIZED,
                    JsonRpcForwardedResponse::from_str(
                        "invalid message eip given",
                        Some(StatusCode::UNAUTHORIZED.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::InvalidInviteCode => {
                warn!("InvalidInviteCode");
                (
                    StatusCode::UNAUTHORIZED,
                    JsonRpcForwardedResponse::from_str(
                        "invalid invite code",
                        Some(StatusCode::UNAUTHORIZED.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::InvalidReferralCode => {
                warn!("InvalidReferralCode");
                (
                    StatusCode::UNAUTHORIZED,
                    JsonRpcForwardedResponse::from_str(
                        "invalid referral code",
                        Some(StatusCode::UNAUTHORIZED.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::InvalidReferer => {
                warn!("InvalidReferer");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        "invalid referer!",
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::InvalidSignatureLength => {
                warn!("InvalidSignatureLength");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        "invalid signature length",
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::InvalidUserAgent => {
                warn!("InvalidUserAgent");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        "invalid user agent!",
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::InvalidUserKey => {
                warn!("InvalidUserKey");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        "UserKey was not a ULID or UUID",
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::JoinError(err) => {
                let code = if err.is_cancelled() {
                    trace!("JoinError. likely shutting down. err={:?}", err);
                    StatusCode::BAD_GATEWAY
                } else {
                    warn!("JoinError. err={:?}", err);
                    StatusCode::INTERNAL_SERVER_ERROR
                };

                (
                    code,
                    JsonRpcForwardedResponse::from_str(
                        // TODO: different messages, too?
                        "Unable to complete request",
                        Some(code.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::MsgPackEncode(err) => {
                warn!("MsgPackEncode Error: {}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcForwardedResponse::from_str(
                        &format!("msgpack encode error: {}", err),
                        Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::NoBlockNumberOrHash => {
                warn!("NoBlockNumberOrHash");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcForwardedResponse::from_str(
                        "Blocks here must have a number or hash",
                        Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::NoBlocksKnown => {
                error!("NoBlocksKnown");
                (
                    StatusCode::BAD_GATEWAY,
                    JsonRpcForwardedResponse::from_str(
                        "no blocks known",
                        Some(StatusCode::BAD_GATEWAY.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::NoConsensusHeadBlock => {
                error!("NoConsensusHeadBlock");
                (
                    StatusCode::BAD_GATEWAY,
                    JsonRpcForwardedResponse::from_str(
                        "no consensus head block",
                        Some(StatusCode::BAD_GATEWAY.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::NoHandleReady => {
                error!("NoHandleReady");
                (
                    StatusCode::BAD_GATEWAY,
                    JsonRpcForwardedResponse::from_str(
                        "unable to retry for request handle",
                        Some(StatusCode::BAD_GATEWAY.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::NoServersSynced => {
                warn!("NoServersSynced");
                (
                    StatusCode::BAD_GATEWAY,
                    JsonRpcForwardedResponse::from_str(
                        "no servers synced",
                        Some(StatusCode::BAD_GATEWAY.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::NotEnoughRpcs {
                num_known,
                min_head_rpcs,
            } => {
                error!("NotEnoughRpcs {}/{}", num_known, min_head_rpcs);
                (
                    StatusCode::BAD_GATEWAY,
                    JsonRpcForwardedResponse::from_string(
                        format!("not enough rpcs connected {}/{}", num_known, min_head_rpcs),
                        Some(StatusCode::BAD_GATEWAY.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::NotFound => {
                // TODO: emit a stat?
                // TODO: instead of an error, show a normal html page for 404?
                (
                    StatusCode::NOT_FOUND,
                    JsonRpcForwardedResponse::from_str(
                        "not found!",
                        Some(StatusCode::NOT_FOUND.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::NotImplemented => {
                trace!("NotImplemented");
                (
                    StatusCode::NOT_IMPLEMENTED,
                    JsonRpcForwardedResponse::from_str(
                        "work in progress",
                        Some(StatusCode::NOT_IMPLEMENTED.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::OriginRequired => {
                trace!("OriginRequired");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        "Origin required",
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::OriginNotAllowed(origin) => {
                trace!("OriginNotAllowed origin={}", origin);
                (
                    StatusCode::FORBIDDEN,
                    JsonRpcForwardedResponse::from_string(
                        format!("Origin ({}) is not allowed!", origin),
                        Some(StatusCode::FORBIDDEN.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::ParseBytesError(err) => {
                trace!("ParseBytesError err={:?}", err);
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        "parse bytes error!",
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::ParseMsgError(err) => {
                trace!("ParseMsgError err={:?}", err);
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        "parse message error!",
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::ParseAddressError => {
                trace!("ParseAddressError");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        "unable to parse address",
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            // TODO: this should actually by the id of the key. multiple users might control one key
            Self::RateLimited(authorization, retry_at) => {
                // TODO: emit a stat

                let retry_msg = if let Some(retry_at) = retry_at {
                    let retry_in = retry_at.duration_since(Instant::now()).as_secs();

                    format!(" Retry in {} seconds", retry_in)
                } else {
                    "".to_string()
                };

                // create a string with either the IP or the rpc_key_id
                let msg = if authorization.checks.rpc_secret_key_id.is_none() {
                    format!("too many requests from {}.{}", authorization.ip, retry_msg)
                } else {
                    format!(
                        "too many requests from rpc key #{}.{}",
                        authorization.checks.rpc_secret_key_id.unwrap(),
                        retry_msg
                    )
                };

                (
                    StatusCode::TOO_MANY_REQUESTS,
                    JsonRpcForwardedResponse::from_string(
                        msg,
                        Some(StatusCode::TOO_MANY_REQUESTS.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::Redis(err) => {
                warn!("redis err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcForwardedResponse::from_str(
                        "redis error!",
                        Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::RefererRequired => {
                warn!("referer required");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        "Referer required",
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::RefererNotAllowed(referer) => {
                warn!("referer not allowed referer={:?}", referer);
                (
                    StatusCode::FORBIDDEN,
                    JsonRpcForwardedResponse::from_string(
                        format!("Referer ({:?}) is not allowed", referer),
                        Some(StatusCode::FORBIDDEN.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::Arc(err) => match Arc::try_unwrap(err) {
                Ok(err) => err,
                Err(err) => Self::Anyhow(anyhow::anyhow!("{}", err)),
            }
            .into_response_parts(),
            Self::SemaphoreAcquireError(err) => {
                warn!("semaphore acquire err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcForwardedResponse::from_string(
                        // TODO: is it safe to expose all of our anyhow strings?
                        "semaphore acquire error".to_string(),
                        Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::SendAppStatError(err) => {
                error!("SendAppStatError err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcForwardedResponse::from_str(
                        "error stat_sender sending response_stat",
                        Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::SerdeJson(err) => {
                warn!("serde json err={:?}", err);
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        "de/serialization error!",
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::StatusCode(status_code, err_msg, err) => {
                // different status codes should get different error levels. 500s should warn. 400s should stat
                let code = status_code.as_u16();
                if (500..600).contains(&code) {
                    warn!("server error {} {:?}: {:?}", code, err_msg, err);
                } else {
                    trace!("user error {} {:?}: {:?}", code, err_msg, err);
                }

                (
                    status_code,
                    JsonRpcForwardedResponse::from_str(&err_msg, Some(code.into()), None),
                )
            }
            Self::Timeout(x) => (
                StatusCode::REQUEST_TIMEOUT,
                JsonRpcForwardedResponse::from_str(
                    &format!("request timed out: {:?}", x),
                    Some(StatusCode::REQUEST_TIMEOUT.as_u16().into()),
                    // TODO: include the actual id!
                    None,
                ),
            ),
            Self::HeaderToString(err) => {
                // trace!(?err, "HeaderToString");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        &format!("{}", err),
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::UlidDecode(err) => {
                // trace!(?err, "UlidDecodeError");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        &format!("{}", err),
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::UnknownBlockNumber => {
                error!("UnknownBlockNumber");
                (
                    StatusCode::BAD_GATEWAY,
                    JsonRpcForwardedResponse::from_str(
                        "no servers synced. unknown eth_blockNumber",
                        Some(StatusCode::BAD_GATEWAY.as_u16().into()),
                        None,
                    ),
                )
            }
            // TODO: stat?
            Self::UnknownKey => (
                StatusCode::UNAUTHORIZED,
                JsonRpcForwardedResponse::from_str(
                    "unknown api key!",
                    Some(StatusCode::UNAUTHORIZED.as_u16().into()),
                    None,
                ),
            ),
            Self::UserAgentRequired => {
                warn!("UserAgentRequired");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        "User agent required",
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::UserAgentNotAllowed(ua) => {
                warn!("UserAgentNotAllowed ua={}", ua);
                (
                    StatusCode::FORBIDDEN,
                    JsonRpcForwardedResponse::from_string(
                        format!("User agent ({}) is not allowed!", ua),
                        Some(StatusCode::FORBIDDEN.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::UserIdZero => {
                warn!("UserIdZero");
                // TODO: this might actually be an application error and not a BAD_REQUEST
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        "user ids should always be non-zero",
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::VerificationError(err) => {
                trace!("VerificationError err={:?}", err);
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        "verification error!",
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::WatchRecvError(err) => {
                error!("WatchRecvError err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcForwardedResponse::from_str(
                        "watch recv error!",
                        Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::WatchSendError => {
                error!("WatchSendError");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcForwardedResponse::from_str(
                        "watch send error!",
                        Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::WebsocketOnly => {
                trace!("WebsocketOnly");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcForwardedResponse::from_str(
                        "redirect_public_url not set. only websockets work here",
                        Some(StatusCode::BAD_REQUEST.as_u16().into()),
                        None,
                    ),
                )
            }
            Self::WithContext(err, msg) => match err {
                Some(err) => {
                    warn!("{:#?} w/ context {}", err, msg);
                    err.into_response_parts()
                }
                None => {
                    warn!("error w/ context {}", msg);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        JsonRpcForwardedResponse::from_string(
                            msg,
                            Some(StatusCode::INTERNAL_SERVER_ERROR.as_u16().into()),
                            None,
                        ),
                    )
                }
            },
        }
    }
}

impl From<ethers::types::ParseBytesError> for Web3ProxyError {
    fn from(err: ethers::types::ParseBytesError) -> Self {
        Self::ParseBytesError(Some(err))
    }
}

impl From<tokio::time::error::Elapsed> for Web3ProxyError {
    fn from(err: tokio::time::error::Elapsed) -> Self {
        Self::Timeout(Some(err))
    }
}

impl IntoResponse for Web3ProxyError {
    fn into_response(self) -> Response {
        // TODO: include the request id in these so that users can give us something that will point to logs
        // TODO: status code is in the jsonrpc response and is also the first item in the tuple. DRY
        let (status_code, response) = self.into_response_parts();

        (status_code, Json(response)).into_response()
    }
}

pub async fn handler_404() -> Response {
    Web3ProxyError::NotFound.into_response()
}

pub trait Web3ProxyErrorContext<T> {
    fn web3_context<S: Into<String>>(self, msg: S) -> Result<T, Web3ProxyError>;
}

impl<T> Web3ProxyErrorContext<T> for Option<T> {
    fn web3_context<S: Into<String>>(self, msg: S) -> Result<T, Web3ProxyError> {
        self.ok_or(Web3ProxyError::WithContext(None, msg.into()))
    }
}

impl<T, E> Web3ProxyErrorContext<T> for Result<T, E>
where
    E: Into<Web3ProxyError>,
{
    fn web3_context<S: Into<String>>(self, msg: S) -> Result<T, Web3ProxyError> {
        self.map_err(|err| Web3ProxyError::WithContext(Some(Box::new(err.into())), msg.into()))
    }
}
