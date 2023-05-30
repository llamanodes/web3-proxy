//! Handle registration, logins, and managing account data.
use crate::app::Web3ProxyApp;
use crate::frontend::errors::{Web3ProxyErrorContext, Web3ProxyResponse};
use crate::http_params::{
    get_chain_id_from_params, get_page_from_params, get_query_start_from_params,
};
use crate::stats::influxdb_queries::query_user_stats;
use crate::stats::StatType;
use axum::{
    extract::Query,
    headers::{authorization::Bearer, Authorization},
    response::IntoResponse,
    Extension, Json, TypedHeader,
};
use axum_macros::debug_handler;
use entities;
use entities::{revert_log, rpc_key};
use hashbrown::HashMap;
use migration::sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder};
use serde_json::json;
use std::sync::Arc;

/// `GET /user/revert_logs` -- Use a bearer token to get the user's revert logs.
#[debug_handler]
pub async fn user_revert_logs_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    TypedHeader(Authorization(bearer)): TypedHeader<Authorization<Bearer>>,
    Query(params): Query<HashMap<String, String>>,
) -> Web3ProxyResponse {
    let (user, _semaphore) = app.bearer_is_authorized(bearer).await?;

    let chain_id = get_chain_id_from_params(app.as_ref(), &params)?;
    let query_start = get_query_start_from_params(&params)?;
    let page = get_page_from_params(&params)?;

    // TODO: page size from config
    let page_size = 1_000;

    let mut response = HashMap::new();

    response.insert("page", json!(page));
    response.insert("page_size", json!(page_size));
    response.insert("chain_id", json!(chain_id));
    response.insert("query_start", json!(query_start.timestamp() as u64));

    let db_replica = app
        .db_replica()
        .web3_context("getting replica db for user's revert logs")?;

    let uks = rpc_key::Entity::find()
        .filter(rpc_key::Column::UserId.eq(user.id))
        .all(db_replica.conn())
        .await
        .web3_context("failed loading user's key")?;

    // TODO: only select the ids
    let uks: Vec<_> = uks.into_iter().map(|x| x.id).collect();

    // get revert logs
    let mut q = revert_log::Entity::find()
        .filter(revert_log::Column::Timestamp.gte(query_start))
        .filter(revert_log::Column::RpcKeyId.is_in(uks))
        .order_by_asc(revert_log::Column::Timestamp);

    if chain_id == 0 {
        // don't do anything
    } else {
        // filter on chain id
        q = q.filter(revert_log::Column::ChainId.eq(chain_id))
    }

    // query the database for number of items and pages
    let pages_result = q
        .clone()
        .paginate(db_replica.conn(), page_size)
        .num_items_and_pages()
        .await?;

    response.insert("num_items", pages_result.number_of_items.into());
    response.insert("num_pages", pages_result.number_of_pages.into());

    // query the database for the revert logs
    let revert_logs = q
        .paginate(db_replica.conn(), page_size)
        .fetch_page(page)
        .await?;

    response.insert("revert_logs", json!(revert_logs));

    Ok(Json(response).into_response())
}

/// `GET /user/stats/aggregate` -- Public endpoint for aggregate stats such as bandwidth used and methods requested.
#[debug_handler]
pub async fn user_stats_aggregated_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    Query(params): Query<HashMap<String, String>>,
) -> Web3ProxyResponse {
    let response = query_user_stats(&app, bearer, &params, StatType::Aggregated).await?;

    Ok(response)
}

/// `GET /user/stats/detailed` -- Use a bearer token to get the user's key stats such as bandwidth used and methods requested.
///
/// If no bearer is provided, detailed stats for all users will be shown.
/// View a single user with `?user_id=$x`.
/// View a single chain with `?chain_id=$x`.
///
/// Set `$x` to zero to see all.
///
/// TODO: this will change as we add better support for secondary users.
#[debug_handler]
pub async fn user_stats_detailed_get(
    Extension(app): Extension<Arc<Web3ProxyApp>>,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    Query(params): Query<HashMap<String, String>>,
) -> Web3ProxyResponse {
    let response = query_user_stats(&app, bearer, &params, StatType::Detailed).await?;

    Ok(response)
}
