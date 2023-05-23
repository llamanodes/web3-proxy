//! Utlities for logging errors for admins and displaying errors to users.

use super::authorization::Authorization;
use crate::jsonrpc::{JsonRpcErrorData, JsonRpcForwardedResponse};
use crate::response_cache::JsonRpcResponseData;

use std::error::Error;
use std::{borrow::Cow, net::IpAddr};

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

impl From<Web3ProxyError> for Web3ProxyResult<()> {
    fn from(value: Web3ProxyError) -> Self {
        Err(value)
    }
}

#[derive(Debug, Display, Error, From)]
pub enum Web3ProxyError {
    AccessDenied,
    #[error(ignore)]
    Anyhow(anyhow::Error),
    #[error(ignore)]
    #[from(ignore)]
    BadRequest(String),
    #[error(ignore)]
    #[from(ignore)]
    BadResponse(String),
    BadRouting,
    Database(DbErr),
    #[display(fmt = "{:#?}, {:#?}", _0, _1)]
    EipVerificationFailed(Box<Web3ProxyError>, Box<Web3ProxyError>),
    EthersHttpClient(ethers::prelude::HttpClientError),
    EthersProvider(ethers::prelude::ProviderError),
    EthersWsClient(ethers::prelude::WsClientError),
    FlumeRecv(flume::RecvError),
    GasEstimateNotU256,
    Headers(headers::Error),
    HeaderToString(ToStrError),
    Hyper(hyper::Error),
    InfluxDb2Request(influxdb2::RequestError),
    #[display(fmt = "{} > {}", min, max)]
    #[from(ignore)]
    InvalidBlockBounds {
        min: u64,
        max: u64,
    },
    InvalidHeaderValue(InvalidHeaderValue),
    InvalidEip,
    InvalidInviteCode,
    Io(std::io::Error),
    UnknownReferralCode,
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
    #[display(fmt = "{}/{}", available, needed)]
    #[from(ignore)]
    NotEnoughSoftLimit {
        available: u32,
        needed: u32,
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
    PaymentRequired,
    VerificationError(siwe::VerificationError),
    WatchRecvError(tokio::sync::watch::error::RecvError),
    WatchSendError,
    WebsocketOnly,
    #[display(fmt = "{:?}, {}", _0, _1)]
    #[error(ignore)]
    WithContext(Option<Box<Web3ProxyError>>, String),
}

impl Web3ProxyError {
    pub fn into_response_parts(self) -> (StatusCode, JsonRpcResponseData) {
        // TODO: include a unique request id in the data
        let (code, err): (StatusCode, JsonRpcErrorData) = match self {
            Self::AccessDenied => {
                // TODO: attach something to this trace. probably don't include much in the message though. don't want to leak creds by accident
                trace!("access denied");
                (
                    StatusCode::FORBIDDEN,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("FORBIDDEN"),
                        code: StatusCode::FORBIDDEN.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::Anyhow(err) => {
                warn!("anyhow. err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcErrorData {
                        // TODO: is it safe to expose all of our anyhow strings?
                        message: Cow::Owned(err.to_string()),
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::BadRequest(err) => {
                debug!("BAD_REQUEST: {}", err);
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcErrorData {
                        message: Cow::Owned(format!("bad request: {}", err)),
                        code: StatusCode::BAD_REQUEST.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::BadResponse(err) => {
                // TODO: think about this one more. ankr gives us this because ethers fails to parse responses without an id
                debug!("BAD_RESPONSE: {}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcErrorData {
                        message: Cow::Owned(format!("bad response: {}", err)),
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::BadRouting => {
                error!("BadRouting");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("bad routing"),
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::Database(err) => {
                error!("database err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("database error!"),
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::EipVerificationFailed(err_1, err_191) => {
                info!(
                    "EipVerificationFailed err_1={:#?} err2={:#?}",
                    err_1, err_191
                );
                (
                    StatusCode::UNAUTHORIZED,
                    JsonRpcErrorData {
                        message: Cow::Owned(format!(
                            "both the primary and eip191 verification failed: {:#?}; {:#?}",
                            err_1, err_191
                        )),
                        code: StatusCode::UNAUTHORIZED.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::EthersHttpClient(err) => {
                warn!("EthersHttpClientError err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("ether http client error"),
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::EthersProvider(err) => {
                warn!("EthersProviderError err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("ether provider error"),
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::EthersWsClient(err) => {
                warn!("EthersWsClientError err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("ether ws client error"),
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::FlumeRecv(err) => {
                warn!("FlumeRecvError err={:#?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("flume recv error!"),
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16().into(),
                        data: None,
                    },
                )
            }
            // Self::JsonRpcForwardedError(x) => (StatusCode::OK, x),
            Self::GasEstimateNotU256 => {
                warn!("GasEstimateNotU256");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("gas estimate result is not an U256"),
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::Headers(err) => {
                warn!("HeadersError {:?}", err);
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcErrorData {
                        message: Cow::Owned(format!("{}", err)),
                        code: StatusCode::BAD_REQUEST.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::Hyper(err) => {
                warn!("hyper err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcErrorData {
                        // TODO: is it safe to expose these error strings?
                        message: Cow::Owned(err.to_string()),
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::InfluxDb2Request(err) => {
                // TODO: attach a request id to the message and to this error so that if people report problems, we can dig in sentry to find out more
                error!("influxdb2 err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("influxdb2 error!"),
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::InvalidBlockBounds { min, max } => {
                debug!("InvalidBlockBounds min={} max={}", min, max);
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcErrorData {
                        message: Cow::Owned(format!(
                            "Invalid blocks bounds requested. min ({}) > max ({})",
                            min, max
                        )),
                        code: StatusCode::BAD_REQUEST.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::IpAddrParse(err) => {
                debug!("IpAddrParse err={:?}", err);
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcErrorData {
                        message: Cow::Owned(err.to_string()),
                        code: StatusCode::BAD_REQUEST.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::IpNotAllowed(ip) => {
                debug!("IpNotAllowed ip={})", ip);
                (
                    StatusCode::FORBIDDEN,
                    JsonRpcErrorData {
                        message: Cow::Owned(format!("IP ({}) is not allowed!", ip)),
                        code: StatusCode::FORBIDDEN.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::InvalidHeaderValue(err) => {
                debug!("InvalidHeaderValue err={:?}", err);
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcErrorData {
                        message: Cow::Owned(format!("{}", err)),
                        code: StatusCode::BAD_REQUEST.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::InvalidEip => {
                debug!("InvalidEip");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("invalid message eip given"),
                        code: StatusCode::BAD_REQUEST.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::InvalidInviteCode => {
                debug!("InvalidInviteCode");
                (
                    StatusCode::UNAUTHORIZED,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("invalid invite code"),
                        code: StatusCode::UNAUTHORIZED.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::Io(err) => {
                warn!("std io err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcErrorData {
                        // TODO: is it safe to expose our io error strings?
                        message: Cow::Owned(err.to_string()),
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::UnknownReferralCode => {
                debug!("UnknownReferralCode");
                (
                    StatusCode::UNAUTHORIZED,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("invalid referral code"),
                        code: StatusCode::UNAUTHORIZED.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::InvalidReferer => {
                debug!("InvalidReferer");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("invalid referer!"),
                        code: StatusCode::BAD_REQUEST.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::InvalidSignatureLength => {
                debug!("InvalidSignatureLength");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("invalid signature length"),
                        code: StatusCode::BAD_REQUEST.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::InvalidUserAgent => {
                debug!("InvalidUserAgent");
                (
                    StatusCode::FORBIDDEN,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("invalid user agent!"),
                        code: StatusCode::FORBIDDEN.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::InvalidUserKey => {
                warn!("InvalidUserKey");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("UserKey was not a ULID or UUID"),
                        code: StatusCode::BAD_REQUEST.as_u16().into(),
                        data: None,
                    },
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
                    JsonRpcErrorData {
                        // TODO: different messages of cancelled or not?
                        message: Cow::Borrowed("Unable to complete request"),
                        code: code.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::MsgPackEncode(err) => {
                warn!("MsgPackEncode Error: {}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcErrorData {
                        message: Cow::Owned(format!("msgpack encode error: {}", err)),
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::NoBlockNumberOrHash => {
                warn!("NoBlockNumberOrHash");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("Blocks here must have a number or hash"),
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::NoBlocksKnown => {
                error!("NoBlocksKnown");
                (
                    StatusCode::BAD_GATEWAY,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("no blocks known"),
                        code: StatusCode::BAD_GATEWAY.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::NoConsensusHeadBlock => {
                error!("NoConsensusHeadBlock");
                (
                    StatusCode::BAD_GATEWAY,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("no consensus head block"),
                        code: StatusCode::BAD_GATEWAY.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::NoHandleReady => {
                error!("NoHandleReady");
                (
                    StatusCode::BAD_GATEWAY,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("unable to retry for request handle"),
                        code: StatusCode::BAD_GATEWAY.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::NoServersSynced => {
                warn!("NoServersSynced");
                (
                    StatusCode::BAD_GATEWAY,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("no servers synced"),
                        code: StatusCode::BAD_GATEWAY.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::NotEnoughRpcs {
                num_known,
                min_head_rpcs,
            } => {
                error!("NotEnoughRpcs {}/{}", num_known, min_head_rpcs);
                (
                    StatusCode::BAD_GATEWAY,
                    JsonRpcErrorData {
                        message: Cow::Owned(format!(
                            "not enough rpcs connected {}/{}",
                            num_known, min_head_rpcs
                        )),
                        code: StatusCode::BAD_GATEWAY.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::NotEnoughSoftLimit { available, needed } => {
                error!("NotEnoughSoftLimit {}/{}", available, needed);
                (
                    StatusCode::BAD_GATEWAY,
                    JsonRpcErrorData {
                        message: Cow::Owned(format!(
                            "not enough soft limit available {}/{}",
                            available, needed
                        )),
                        code: StatusCode::BAD_GATEWAY.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::NotFound => {
                // TODO: emit a stat?
                // TODO: instead of an error, show a normal html page for 404?
                (
                    StatusCode::NOT_FOUND,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("not found!"),
                        code: StatusCode::NOT_FOUND.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::NotImplemented => {
                trace!("NotImplemented");
                (
                    StatusCode::NOT_IMPLEMENTED,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("work in progress"),
                        code: StatusCode::NOT_IMPLEMENTED.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::OriginRequired => {
                trace!("OriginRequired");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("Origin required"),
                        code: StatusCode::BAD_REQUEST.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::OriginNotAllowed(origin) => {
                trace!("OriginNotAllowed origin={}", origin);
                (
                    StatusCode::FORBIDDEN,
                    JsonRpcErrorData {
                        message: Cow::Owned(format!("Origin ({}) is not allowed!", origin)),
                        code: StatusCode::FORBIDDEN.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::ParseBytesError(err) => {
                trace!("ParseBytesError err={:?}", err);
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("parse bytes error!"),
                        code: StatusCode::BAD_REQUEST.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::ParseMsgError(err) => {
                trace!("ParseMsgError err={:?}", err);
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("parse message error!"),
                        code: StatusCode::BAD_REQUEST.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::ParseAddressError => {
                trace!("ParseAddressError");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("unable to parse address"),
                        code: StatusCode::BAD_REQUEST.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::PaymentRequired => {
                trace!("PaymentRequiredError");
                (
                    StatusCode::PAYMENT_REQUIRED,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("Payment is required and user is not premium"),
                        code: StatusCode::PAYMENT_REQUIRED.as_u16().into(),
                        data: None,
                    },
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
                        retry_msg,
                    )
                };

                (
                    StatusCode::TOO_MANY_REQUESTS,
                    JsonRpcErrorData {
                        message: Cow::Owned(msg),
                        code: StatusCode::TOO_MANY_REQUESTS.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::Redis(err) => {
                warn!("redis err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("redis error!"),
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::RefererRequired => {
                warn!("referer required");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("Referer required"),
                        code: StatusCode::BAD_REQUEST.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::RefererNotAllowed(referer) => {
                warn!("referer not allowed referer={:?}", referer);
                (
                    StatusCode::FORBIDDEN,
                    JsonRpcErrorData {
                        message: Cow::Owned(format!("Referer ({:?}) is not allowed", referer)),
                        code: StatusCode::FORBIDDEN.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::SemaphoreAcquireError(err) => {
                warn!("semaphore acquire err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcErrorData {
                        // TODO: is it safe to expose all of our anyhow strings?
                        message: Cow::Borrowed("semaphore acquire error"),
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::SendAppStatError(err) => {
                error!("SendAppStatError err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("error stat_sender sending response_stat"),
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::SerdeJson(err) => {
                warn!("serde json err={:?} source={:?}", err, err.source());
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcErrorData {
                        message: Cow::Owned(format!("de/serialization error! {}", err)),
                        code: StatusCode::BAD_REQUEST.as_u16().into(),
                        data: None,
                    },
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
                    JsonRpcErrorData {
                        message: Cow::Owned(err_msg),
                        code: code.into(),
                        data: None,
                    },
                )
            }
            Self::Timeout(x) => (
                StatusCode::REQUEST_TIMEOUT,
                JsonRpcErrorData {
                    message: Cow::Owned(format!("request timed out: {:?}", x)),
                    code: StatusCode::REQUEST_TIMEOUT.as_u16().into(),
                    // TODO: include the actual id!
                    data: None,
                },
            ),
            Self::HeaderToString(err) => {
                // trace!(?err, "HeaderToString");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcErrorData {
                        message: Cow::Owned(err.to_string()),
                        code: StatusCode::BAD_REQUEST.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::UlidDecode(err) => {
                // trace!(?err, "UlidDecodeError");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcErrorData {
                        message: Cow::Owned(format!("{}", err)),
                        code: StatusCode::BAD_REQUEST.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::UnknownBlockNumber => {
                error!("UnknownBlockNumber");
                (
                    StatusCode::BAD_GATEWAY,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("no servers synced. unknown eth_blockNumber"),
                        code: StatusCode::BAD_GATEWAY.as_u16().into(),
                        data: None,
                    },
                )
            }
            // TODO: stat?
            Self::UnknownKey => (
                StatusCode::UNAUTHORIZED,
                JsonRpcErrorData {
                    message: Cow::Borrowed("unknown api key!"),
                    code: StatusCode::UNAUTHORIZED.as_u16().into(),
                    data: None,
                },
            ),
            Self::UserAgentRequired => {
                warn!("UserAgentRequired");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("User agent required"),
                        code: StatusCode::BAD_REQUEST.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::UserAgentNotAllowed(ua) => {
                warn!("UserAgentNotAllowed ua={}", ua);
                (
                    StatusCode::FORBIDDEN,
                    JsonRpcErrorData {
                        message: Cow::Owned(format!("User agent ({}) is not allowed!", ua)),
                        code: StatusCode::FORBIDDEN.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::UserIdZero => {
                warn!("UserIdZero");
                // TODO: this might actually be an application error and not a BAD_REQUEST
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("user ids should always be non-zero"),
                        code: StatusCode::BAD_REQUEST.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::VerificationError(err) => {
                trace!("VerificationError err={:?}", err);
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("verification error!"),
                        code: StatusCode::BAD_REQUEST.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::WatchRecvError(err) => {
                error!("WatchRecvError err={:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("watch recv error!"),
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::WatchSendError => {
                error!("WatchSendError");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    JsonRpcErrorData {
                        message: Cow::Borrowed("watch send error!"),
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::WebsocketOnly => {
                trace!("WebsocketOnly");
                (
                    StatusCode::BAD_REQUEST,
                    JsonRpcErrorData {
                        message: Cow::Borrowed(
                            "redirect_public_url not set. only websockets work here",
                        ),
                        code: StatusCode::BAD_REQUEST.as_u16().into(),
                        data: None,
                    },
                )
            }
            Self::WithContext(err, msg) => match err {
                Some(err) => {
                    warn!("{:#?} w/ context {}", err, msg);
                    return err.into_response_parts();
                }
                None => {
                    warn!("error w/ context {}", msg);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        JsonRpcErrorData {
                            message: Cow::Owned(msg),
                            code: StatusCode::INTERNAL_SERVER_ERROR.as_u16().into(),
                            data: None,
                        },
                    )
                }
            },
        };

        (code, JsonRpcResponseData::from(err))
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
        let (status_code, response_data) = self.into_response_parts();

        // this will be missing the jsonrpc id!
        // its better to get request id and call from_response_data with it then to use this IntoResponse helper.
        let response =
            JsonRpcForwardedResponse::from_response_data(response_data, Default::default());

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
