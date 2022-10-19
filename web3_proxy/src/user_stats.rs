use entities::rpc_accounting;
use num::Zero;
use sea_orm::{ColumnTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter, QuerySelect};

pub async fn get_aggregate_stats(
    chain_id: u64,
    db: &DatabaseConnection,
    query_start: chrono::NaiveDateTime,
) -> anyhow::Result<Vec<serde_json::Value>> {
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
        );

    let condition = Condition::all().add(rpc_accounting::Column::PeriodDatetime.gte(query_start));

    let (q, condition) = if chain_id.is_zero() {
        // fetch all the chains. don't filter
        let q = q
            .column(rpc_accounting::Column::ChainId)
            .group_by(rpc_accounting::Column::ChainId);

        (q, condition)
    } else {
        let condition = condition.add(rpc_accounting::Column::ChainId.eq(chain_id));

        (q, condition)
    };

    let q = q.filter(condition);

    // TODO: if user key id is set, use that
    // TODO: if user id is set, use that
    // TODO: handle secondary users, too

    let r = q.into_json().all(db).await?;

    Ok(r)
}
