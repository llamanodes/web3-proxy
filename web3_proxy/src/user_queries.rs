use anyhow::Context;
use axum::{
    headers::{authorization::Bearer, Authorization},
    TypedHeader,
};
use chrono::NaiveDateTime;
use entities::{rpc_accounting, rpc_keys};
use hashbrown::HashMap;
use migration::Expr;
use num::Zero;
use redis_rate_limiter::{redis::AsyncCommands, RedisConnection};
use sea_orm::{
    ColumnTrait, Condition, EntityTrait, JoinType, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect, RelationTrait,
};
use tracing::{instrument, trace};

use crate::{app::Web3ProxyApp, user_token::UserBearerToken};

/// get the attached address from redis for the given auth_token.
/// 0 means all users
#[instrument(level = "trace", skip(redis_conn))]
async fn get_user_id_from_params(
    mut redis_conn: RedisConnection,
    // this is a long type. should we strip it down?
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    params: &HashMap<String, String>,
) -> anyhow::Result<u64> {
    match (bearer, params.get("user_id")) {
        (Some(TypedHeader(Authorization(bearer))), Some(user_id)) => {
            // check for the bearer cache key
            let bearer_cache_key = UserBearerToken::try_from(bearer)?.to_string();

            // get the user id that is attached to this bearer token
            redis_conn
                .get::<_, u64>(bearer_cache_key)
                .await
                // TODO: this should be a 403
                .context("fetching rpc_key_id from redis with bearer_cache_key")
        }
        (_, None) => {
            // they have a bearer token. we don't care about it on public pages
            // 0 means all
            Ok(0)
        }
        (None, Some(x)) => {
            // they do not have a bearer token, but requested a specific id. block
            // TODO: proper error code
            // TODO: maybe instead of this sharp edged warn, we have a config value?
            // TODO: check config for if we should deny or allow this
            x.parse().context("Parsing user_id param")
        }
    }
}

/// only allow rpc_key to be set if user_id is also set.
/// this will keep people from reading someone else's keys.
/// 0 means none.
#[instrument(level = "trace")]
pub fn get_rpc_key_id_from_params(
    user_id: u64,
    params: &HashMap<String, String>,
) -> anyhow::Result<u64> {
    if user_id > 0 {
        params.get("rpc_key_id").map_or_else(
            || Ok(0),
            |c| {
                let c = c.parse()?;

                Ok(c)
            },
        )
    } else {
        Ok(0)
    }
}

#[instrument(level = "trace")]
pub fn get_chain_id_from_params(
    app: &Web3ProxyApp,
    params: &HashMap<String, String>,
) -> anyhow::Result<u64> {
    params.get("chain_id").map_or_else(
        || Ok(app.config.chain_id),
        |c| {
            let c = c.parse()?;

            Ok(c)
        },
    )
}

#[instrument(level = "trace")]
pub fn get_query_start_from_params(
    params: &HashMap<String, String>,
) -> anyhow::Result<chrono::NaiveDateTime> {
    params.get("query_start").map_or_else(
        || {
            // no timestamp in params. set default
            let x = chrono::Utc::now() - chrono::Duration::days(30);

            Ok(x.naive_utc())
        },
        |x: &String| {
            // parse the given timestamp
            let x = x.parse::<i64>().context("parsing timestamp query param")?;

            // TODO: error code 401
            let x =
                NaiveDateTime::from_timestamp_opt(x, 0).context("parsing timestamp query param")?;

            Ok(x)
        },
    )
}

#[instrument(level = "trace")]
pub fn get_page_from_params(params: &HashMap<String, String>) -> anyhow::Result<u64> {
    params.get("page").map_or_else::<anyhow::Result<u64>, _, _>(
        || {
            // no page in params. set default
            Ok(0)
        },
        |x: &String| {
            // parse the given timestamp
            // TODO: error code 401
            let x = x.parse().context("parsing page query from params")?;

            Ok(x)
        },
    )
}

#[instrument(level = "trace")]
pub fn get_query_window_seconds_from_params(
    params: &HashMap<String, String>,
) -> anyhow::Result<u64> {
    params.get("query_window_seconds").map_or_else(
        || {
            // no page in params. set default
            Ok(0)
        },
        |x: &String| {
            // parse the given timestamp
            // TODO: error code 401
            let x = x
                .parse()
                .context("parsing query window seconds from params")?;

            Ok(x)
        },
    )
}

/// stats aggregated across a large time period
#[instrument(level = "trace")]
pub async fn get_aggregate_rpc_stats_from_params(
    app: &Web3ProxyApp,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    params: HashMap<String, String>,
) -> anyhow::Result<HashMap<&str, serde_json::Value>> {
    let db_conn = app.db_conn().context("connecting to db")?;
    let redis_conn = app.redis_conn().await.context("connecting to redis")?;

    let user_id = get_user_id_from_params(redis_conn, bearer, &params).await?;
    let chain_id = get_chain_id_from_params(app, &params)?;
    let query_start = get_query_start_from_params(&params)?;
    let query_window_seconds = get_query_window_seconds_from_params(&params)?;
    let page = get_page_from_params(&params)?;

    // TODO: warn if unknown fields in params

    // TODO: page size from config
    let page_size = 200;

    trace!(?chain_id, %query_start, ?user_id, "get_aggregate_stats");

    // TODO: minimum query_start of 90 days?

    let mut response = HashMap::new();

    response.insert("page", serde_json::to_value(page)?);
    response.insert("page_size", serde_json::to_value(page_size)?);
    response.insert("chain_id", serde_json::to_value(chain_id)?);
    response.insert(
        "query_start",
        serde_json::to_value(query_start.timestamp() as u64)?,
    );

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

    // TODO: DRYer
    let q = if query_window_seconds != 0 {
        /*
        let query_start_timestamp: u64 = query_start
            .timestamp()
            .try_into()
            .context("query_start to timestamp")?;
        */
        // TODO: is there a better way to do this? how can we get "period_datetime" into this with types?
        // TODO: how can we get the first window to start at query_start_timestamp
        let expr = Expr::cust_with_values(
            "FLOOR(UNIX_TIMESTAMP(rpc_accounting.period_datetime) / ?) * ?",
            [query_window_seconds, query_window_seconds],
        );

        response.insert(
            "query_window_seconds",
            serde_json::to_value(query_window_seconds)?,
        );

        q.column_as(expr, "query_window_seconds")
            .group_by(Expr::cust("query_window_seconds"))
    } else {
        // TODO: order by more than this?
        // query_window_seconds is not set so we aggregate all records
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
            .join(JoinType::InnerJoin, rpc_accounting::Relation::RpcKeys.def())
            .column(rpc_keys::Column::UserId)
            .group_by(rpc_keys::Column::UserId);

        let condition = condition.add(rpc_keys::Column::UserId.eq(user_id));

        (condition, q)
    };

    let q = q.filter(condition);

    // TODO: enum between searching on rpc_key_id on user_id
    // TODO: handle secondary users, too

    // log query here. i think sea orm has a useful log level for this

    let aggregate = q
        .into_json()
        .paginate(&db_conn, page_size)
        .fetch_page(page)
        .await?;

    response.insert("aggregate", serde_json::Value::Array(aggregate));

    Ok(response)
}

/// stats grouped by key_id and error_repsponse and method and key
#[instrument(level = "trace")]
pub async fn get_detailed_stats(
    app: &Web3ProxyApp,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    params: HashMap<String, String>,
) -> anyhow::Result<HashMap<&str, serde_json::Value>> {
    let db_conn = app.db_conn().context("connecting to db")?;
    let redis_conn = app.redis_conn().await.context("connecting to redis")?;

    let user_id = get_user_id_from_params(redis_conn, bearer, &params).await?;
    let rpc_key_id = get_rpc_key_id_from_params(user_id, &params)?;
    let chain_id = get_chain_id_from_params(app, &params)?;
    let query_start = get_query_start_from_params(&params)?;
    let query_window_seconds = get_query_window_seconds_from_params(&params)?;
    let page = get_page_from_params(&params)?;
    // TODO: handle secondary users, too

    // TODO: page size from config
    let page_size = 200;

    // TODO: minimum query_start of 90 days?

    let mut response = HashMap::new();

    response.insert("page", serde_json::to_value(page)?);
    response.insert("page_size", serde_json::to_value(page_size)?);
    response.insert("chain_id", serde_json::to_value(chain_id)?);
    response.insert(
        "query_start",
        serde_json::to_value(query_start.timestamp() as u64)?,
    );

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

    let (condition, q) = if user_id == 0 {
        // 0 means everyone. don't filter on user
        (condition, q)
    } else {
        // TODO: move authentication here?
        // TODO: what about keys where this user is a secondary user?
        let q = q
            .join(JoinType::InnerJoin, rpc_accounting::Relation::RpcKeys.def())
            .column(rpc_keys::Column::UserId)
            .group_by(rpc_keys::Column::UserId);

        let condition = condition.add(rpc_keys::Column::UserId.eq(user_id));

        let q = if rpc_key_id == 0 {
            q.column(rpc_keys::Column::UserId)
                .group_by(rpc_keys::Column::UserId)
        } else {
            response.insert("rpc_key_id", serde_json::to_value(rpc_key_id)?);

            // no need to group_by user_id when we are grouping by key_id
            q.column(rpc_keys::Column::Id)
                .group_by(rpc_keys::Column::Id)
        };

        (condition, q)
    };

    let q = if query_window_seconds != 0 {
        /*
        let query_start_timestamp: u64 = query_start
            .timestamp()
            .try_into()
            .context("query_start to timestamp")?;
        */
        // TODO: is there a better way to do this? how can we get "period_datetime" into this with types?
        // TODO: how can we get the first window to start at query_start_timestamp
        let expr = Expr::cust_with_values(
            "FLOOR(UNIX_TIMESTAMP(rpc_accounting.period_datetime) / ?) * ?",
            [query_window_seconds, query_window_seconds],
        );

        response.insert(
            "query_window_seconds",
            serde_json::to_value(query_window_seconds)?,
        );

        q.column_as(expr, "query_window_seconds")
            .group_by(Expr::cust("query_window_seconds"))
    } else {
        // TODO: order by more than this?
        // query_window_seconds is not set so we aggregate all records
        q
    };

    let q = q.filter(condition);

    // log query here. i think sea orm has a useful log level for this

    // TODO: transform this into a nested hashmap instead of a giant table?
    let r = q
        .into_json()
        .paginate(&db_conn, page_size)
        .fetch_page(page)
        .await?;

    response.insert("detailed_aggregate", serde_json::Value::Array(r));

    // number of keys
    // number of secondary keys
    // avg and max concurrent requests per second per api key

    Ok(response)
}
