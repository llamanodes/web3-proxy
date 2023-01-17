//! Handle admin helper logic

use super::authorization::{login_is_authorized, RpcSecretKey};
use super::errors::FrontendResult;
use crate::app::Web3ProxyApp;
use crate::user_queries::{get_page_from_params, get_user_id_from_params};
use crate::user_queries::{
    get_chain_id_from_params, get_query_start_from_params, query_user_stats, StatResponse,
};
use entities::prelude::{User, SecondaryUser};
use crate::user_token::UserBearerToken;
use anyhow::Context;
use axum::headers::{Header, Origin, Referer, UserAgent};
use axum::{
    extract::{Path, Query},
    headers::{authorization::Bearer, Authorization},
    response::IntoResponse,
    Extension, Json, TypedHeader,
};
use axum_client_ip::ClientIp;
use axum_macros::debug_handler;
use chrono::{TimeZone, Utc};
use entities::sea_orm_active_enums::{LogLevel, Role};
use entities::{login, pending_login, revert_log, rpc_key, secondary_user, user, user_tier};
use ethers::{prelude::Address, types::Bytes};
use hashbrown::HashMap;
use http::{HeaderValue, StatusCode};
use ipnet::IpNet;
use itertools::Itertools;
use log::{debug, info, warn};
use migration::sea_orm::prelude::Uuid;
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, PaginatorTrait, QueryFilter,
    QueryOrder, TransactionTrait, TryIntoModel,
};
use serde::Deserialize;
use serde_json::json;
use siwe::{Message, VerificationOpts};
use std::ops::Add;
use std::str::FromStr;
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
use ulid::Ulid;
use crate::admin_queries::query_admin_modify_usertier;
use crate::frontend::errors::FrontendErrorResponse;

/// `GET /admin/modify_role` -- Use a bearer token to get the user's key stats such as bandwidth used and methods requested.
///
/// If no bearer is provided, detailed stats for all users will be shown.
/// View a single user with `?user_id=$x`.
/// View a single chain with `?chain_id=$x`.
///
/// Set `$x` to zero to see all.
///
/// TODO: this will change as we add better support for secondary users.
#[debug_handler]
pub async fn admin_change_user_roles(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    Query(params): Query<HashMap<String, String>>,
) -> FrontendResult {
    let response = query_admin_modify_usertier(&app, bearer, &params).await?;

    Ok(response)
}
