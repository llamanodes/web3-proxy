use anyhow::Context;
use entities::{rpc_accounting, user_keys};
use hashbrown::HashMap;
use migration::Expr;
use num::Zero;
use sea_orm::{
    ColumnTrait, Condition, DatabaseConnection, EntityTrait, JoinType, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect, RelationTrait,
};
use tracing::trace;

/// stats aggregated across a large time period
pub async fn get_aggregate_rpc_stats(
    chain_id: u64,
    db_conn: &DatabaseConnection,
    page: usize,
    page_size: usize,
    query_start: chrono::NaiveDateTime,
    query_window_seconds: Option<u64>,
    user_id: u64,
) -> anyhow::Result<HashMap<&str, serde_json::Value>> {
    trace!(?chain_id, %query_start, ?user_id, "get_aggregate_stats");

    // TODO: minimum query_start of 90 days?

    let mut response = HashMap::new();

    response.insert("page", serde_json::to_value(page)?);
    response.insert("page_size", serde_json::to_value(page_size)?);
    response.insert("chain_id", serde_json::to_value(chain_id)?);
    response.insert("query_start", serde_json::to_value(query_start)?);

    // TODO: how do we get count reverts compared to other errors? does it matter? what about http errors to our users?
    // TODO: how do we count uptime?
    let q = rpc_accounting::Entity::find()
        .select_only()
        .column_as(
            rpc_accounting::Column::FrontendRequests.sum(),
            "total_requests",
        )
        .column_as(
            rpc_accounting::Column::CacheMisses.sum(),
            "total_cache_misses",
        )
        .column_as(rpc_accounting::Column::CacheHits.sum(), "total_cache_hits")
        .column_as(
            rpc_accounting::Column::BackendRetries.sum(),
            "total_backend_retries",
        )
        .column_as(
            rpc_accounting::Column::SumResponseBytes.sum(),
            "total_response_bytes",
        )
        .column_as(
            // TODO: can we sum bools like this?
            rpc_accounting::Column::ErrorResponse.sum(),
            "total_error_responses",
        )
        .column_as(
            rpc_accounting::Column::SumResponseMillis.sum(),
            "total_response_millis",
        )
        .order_by_asc(rpc_accounting::Column::PeriodDatetime.min());

    let q = if let Some(query_window_seconds) = query_window_seconds {
        debug_assert_ne!(query_window_seconds, 0);

        // TODO: is there a better way to do this? how can we get "period_datetime" into this with types?
        let expr = Expr::cust_with_values(
            "FLOOR(UNIX_TIMESTAMP(rpc_accounting.period_datetime) / ?) * ?",
            [query_window_seconds, query_window_seconds],
        );

        response.insert(
            "query_window_seconds",
            serde_json::to_value(query_window_seconds)?,
        );

        q.column_as(expr, "query_window")
            .group_by(Expr::cust("query_window"))
    } else {
        // TODO: order by more than this?
        // query_window_seconds
        q
    };

    let condition = Condition::all().add(rpc_accounting::Column::PeriodDatetime.gte(query_start));

    let (condition, q) = if chain_id.is_zero() {
        // fetch all the chains. don't filter
        // TODO: wait. do we want chain id on the logs? we can get that by joining key
        let q = q
            .column(rpc_accounting::Column::ChainId)
            .group_by(rpc_accounting::Column::ChainId);

        (condition, q)
    } else {
        let condition = condition.add(rpc_accounting::Column::ChainId.eq(chain_id));

        (condition, q)
    };

    let (condition, q) = if user_id.is_zero() {
        // 0 means everyone. don't filter on user
        (condition, q)
    } else {
        // TODO: authentication here? or should that be higher in the stack? here sems safest
        // TODO: only join some columns
        // TODO: are these joins correct?
        // TODO: what about keys where they are the secondary users?
        let q = q
            .join(
                JoinType::InnerJoin,
                rpc_accounting::Relation::UserKeys.def(),
            )
            .column(user_keys::Column::UserId)
            .group_by(user_keys::Column::UserId);

        let condition = condition.add(user_keys::Column::UserId.eq(user_id));

        (condition, q)
    };

    let q = q.filter(condition);

    // TODO: enum between searching on user_key_id on user_id
    // TODO: handle secondary users, too

    // log query here. i think sea orm has a useful log level for this

    let aggregate = q
        .into_json()
        .paginate(db_conn, page_size)
        .fetch_page(page)
        .await?;

    response.insert("aggregrate", serde_json::Value::Array(aggregate));

    Ok(response)
}

pub async fn get_user_stats(chain_id: u64) -> u64 {
    todo!();
}

/// stats grouped by key_id and error_repsponse and method and key
///
/// TODO: take a "timebucket" duration in minutes that will make a more advanced
pub async fn get_detailed_stats(
    chain_id: u64,
    db_conn: &DatabaseConnection,
    page: usize,
    page_size: usize,
    query_start: chrono::NaiveDateTime,
    user_key_id: u64,
    user_id: u64,
) -> anyhow::Result<HashMap<&str, serde_json::Value>> {
    // aggregate stats, but grouped by method and error
    trace!(?chain_id, %query_start, ?user_id, "get_aggregate_stats");

    // TODO: minimum query_start of 90 days?

    let mut response = HashMap::new();

    response.insert("page", serde_json::to_value(page)?);
    response.insert("page_size", serde_json::to_value(page_size)?);
    response.insert("chain_id", serde_json::to_value(chain_id)?);
    response.insert("query_start", serde_json::to_value(query_start)?);

    // TODO: how do we get count reverts compared to other errors? does it matter? what about http errors to our users?
    // TODO: how do we count uptime?
    let q = rpc_accounting::Entity::find()
        .select_only()
        // groups
        .column(rpc_accounting::Column::ErrorResponse)
        .group_by(rpc_accounting::Column::ErrorResponse)
        .column(rpc_accounting::Column::Method)
        .group_by(rpc_accounting::Column::Method)
        // aggregate columns
        .column_as(
            rpc_accounting::Column::FrontendRequests.sum(),
            "total_requests",
        )
        .column_as(
            rpc_accounting::Column::CacheMisses.sum(),
            "total_cache_misses",
        )
        .column_as(rpc_accounting::Column::CacheHits.sum(), "total_cache_hits")
        .column_as(
            rpc_accounting::Column::BackendRetries.sum(),
            "total_backend_retries",
        )
        .column_as(
            rpc_accounting::Column::SumResponseBytes.sum(),
            "total_response_bytes",
        )
        .column_as(
            // TODO: can we sum bools like this?
            rpc_accounting::Column::ErrorResponse.sum(),
            "total_error_responses",
        )
        .column_as(
            rpc_accounting::Column::SumResponseMillis.sum(),
            "total_response_millis",
        )
        // TODO: order on method next?
        .order_by_asc(rpc_accounting::Column::PeriodDatetime.min());

    let condition = Condition::all().add(rpc_accounting::Column::PeriodDatetime.gte(query_start));

    let (condition, q) = if chain_id.is_zero() {
        // fetch all the chains. don't filter
        // TODO: wait. do we want chain id on the logs? we can get that by joining key
        let q = q
            .column(rpc_accounting::Column::ChainId)
            .group_by(rpc_accounting::Column::ChainId);

        (condition, q)
    } else {
        let condition = condition.add(rpc_accounting::Column::ChainId.eq(chain_id));

        (condition, q)
    };

    let (condition, q) = if user_id.is_zero() {
        // 0 means everyone. don't filter on user
        (condition, q)
    } else {
        // TODO: move authentication here?
        // TODO: what about keys where this user is a secondary user?
        let q = q
            .join(
                JoinType::InnerJoin,
                rpc_accounting::Relation::UserKeys.def(),
            )
            .column(user_keys::Column::UserId)
            // no need to group_by user_id when we are grouping by key_id
            // .group_by(user_keys::Column::UserId)
            .column(user_keys::Column::Id)
            .group_by(user_keys::Column::Id);

        let condition = condition.add(user_keys::Column::UserId.eq(user_id));

        (condition, q)
    };

    let q = q.filter(condition);

    // TODO: enum between searching on user_key_id on user_id
    // TODO: handle secondary users, too

    // log query here. i think sea orm has a useful log level for this

    // TODO: transform this into a nested hashmap instead of a giant table?
    let r = q
        .into_json()
        .paginate(db_conn, page_size)
        .fetch_page(page)
        .await?;

    response.insert("detailed_aggregate", serde_json::Value::Array(r));

    // number of keys
    // number of secondary keys
    // avg and max concurrent requests per second per api key

    Ok(response)
}

/// revert logs for a single key
///
/// TODO: take a "timebucket" duration in minutes that will make a more advanced
pub async fn get_revert_logs(
    chain_id: u64,
    db_conn: &DatabaseConnection,
    page: usize,
    page_size: usize,
    query_start: chrono::NaiveDateTime,
    user_id: u64,
    key_id: u64,
) -> anyhow::Result<HashMap<&str, serde_json::Value>> {
    // aggregate stats, but grouped by method and error
    trace!(?chain_id, %query_start, ?user_id, "get_aggregate_stats");

    // TODO: minimum query_start of 90 days?

    let mut response = HashMap::new();

    response.insert("page", serde_json::to_value(page)?);
    response.insert("page_size", serde_json::to_value(page_size)?);
    response.insert("chain_id", serde_json::to_value(chain_id)?);
    response.insert("query_start", serde_json::to_value(query_start)?);

    // TODO: how do we get count reverts compared to other errors? does it matter? what about http errors to our users?
    // TODO: how do we count uptime?
    let q = rpc_accounting::Entity::find()
        .select_only()
        // groups
        .column(rpc_accounting::Column::ErrorResponse)
        .group_by(rpc_accounting::Column::ErrorResponse)
        .column(rpc_accounting::Column::Method)
        .group_by(rpc_accounting::Column::Method)
        // aggregate columns
        .column_as(
            rpc_accounting::Column::FrontendRequests.sum(),
            "total_requests",
        )
        .column_as(
            rpc_accounting::Column::CacheMisses.sum(),
            "total_cache_misses",
        )
        .column_as(rpc_accounting::Column::CacheHits.sum(), "total_cache_hits")
        .column_as(
            rpc_accounting::Column::BackendRetries.sum(),
            "total_backend_retries",
        )
        .column_as(
            rpc_accounting::Column::SumResponseBytes.sum(),
            "total_response_bytes",
        )
        .column_as(
            // TODO: can we sum bools like this?
            rpc_accounting::Column::ErrorResponse.sum(),
            "total_error_responses",
        )
        .column_as(
            rpc_accounting::Column::SumResponseMillis.sum(),
            "total_response_millis",
        )
        // TODO: order on method next?
        .order_by_asc(rpc_accounting::Column::PeriodDatetime.min());

    let condition = Condition::all().add(rpc_accounting::Column::PeriodDatetime.gte(query_start));

    let (condition, q) = if chain_id.is_zero() {
        // fetch all the chains. don't filter
        // TODO: wait. do we want chain id on the logs? we can get that by joining key
        let q = q
            .column(rpc_accounting::Column::ChainId)
            .group_by(rpc_accounting::Column::ChainId);

        (condition, q)
    } else {
        let condition = condition.add(rpc_accounting::Column::ChainId.eq(chain_id));

        (condition, q)
    };

    let (condition, q) = if user_id.is_zero() {
        // 0 means everyone. don't filter on user
        (condition, q)
    } else {
        // TODO: move authentication here?
        // TODO: what about keys where this user is a secondary user?
        let q = q
            .join(
                JoinType::InnerJoin,
                rpc_accounting::Relation::UserKeys.def(),
            )
            .column(user_keys::Column::UserId)
            // no need to group_by user_id when we are grouping by key_id
            // .group_by(user_keys::Column::UserId)
            .column(user_keys::Column::Id)
            .group_by(user_keys::Column::Id);

        let condition = condition.add(user_keys::Column::UserId.eq(user_id));

        (condition, q)
    };

    let q = q.filter(condition);

    // TODO: enum between searching on user_key_id on user_id
    // TODO: handle secondary users, too

    // log query here. i think sea orm has a useful log level for this

    // TODO: transform this into a nested hashmap instead of a giant table?
    let r = q
        .into_json()
        .paginate(db_conn, page_size)
        .fetch_page(page)
        .await?;

    response.insert("detailed_aggregate", serde_json::Value::Array(r));

    // number of keys
    // number of secondary keys
    // avg and max concurrent requests per second per api key

    Ok(response)
}
