use entities::{rpc_accounting, user, user_keys};
use num::Zero;
use sea_orm::{
    ColumnTrait, Condition, DatabaseConnection, EntityTrait, JoinType, QueryFilter, QuerySelect,
    RelationTrait,
};
use tracing::debug;

pub async fn get_aggregate_stats(
    chain_id: u64,
    db: &DatabaseConnection,
    query_start: chrono::NaiveDateTime,
    user_id: Option<u64>,
) -> anyhow::Result<Vec<serde_json::Value>> {
    debug!(?chain_id, %query_start, ?user_id, "get_aggregate_stats");

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

    /*
    let (q, condition) = if chain_id.is_zero() {
        // fetch all the chains. don't filter
        // TODO: wait. do we want chain id on the logs? we can get that by joining key
        let q = q
            .column(rpc_accounting::Column::ChainId)
            .group_by(rpc_accounting::Column::ChainId);

        (q, condition)
    } else {
        let condition = condition.add(rpc_accounting::Column::ChainId.eq(chain_id));

        (q, condition)
    };
     */

    let q = q.filter(condition);

    // // TODO: also check secondary users
    // let q = if let Some(user_id) = user_id {
    //     // TODO: authentication here? or should that be higher in the stack? here sems safest
    //     // TODO: only join some columns
    //     // TODO: are these joins correct?
    //     q.join(
    //         JoinType::InnerJoin,
    //         rpc_accounting::Relation::UserKeys.def(),
    //     )
    //     .join(JoinType::InnerJoin, user_keys::Relation::User.def())
    //     .filter(user::Column::Id.eq(user_id))
    // } else {
    //     q
    // };

    // TODO: if user key id is set, use that
    // TODO: if user id is set, use that
    // TODO: handle secondary users, too

    let r = q.into_json().all(db).await?;

    Ok(r)
}
