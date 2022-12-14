use crate::frontend::errors::FrontendErrorResponse;
use crate::{app::Web3ProxyApp, user_token::UserBearerToken};
use anyhow::Context;
use axum::{
    headers::{authorization::Bearer, Authorization},
    TypedHeader,
};
use chrono::{NaiveDateTime, Utc};
use entities::{login, rpc_accounting, rpc_key};
use hashbrown::HashMap;
use http::StatusCode;
use log::{debug, warn};
use migration::sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect, Select,
};
use migration::{Condition, Expr, SimpleExpr};
use redis_rate_limiter::{redis::AsyncCommands, RedisConnection};

/// get the attached address for the given bearer token.
/// First checks redis. Then checks the database.
/// 0 means all users
pub async fn get_user_id_from_params(
    mut redis_conn: RedisConnection,
    db_conn: DatabaseConnection,
    // this is a long type. should we strip it down?
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    params: &HashMap<String, String>,
) -> Result<u64, FrontendErrorResponse> {
    match (bearer, params.get("user_id")) {
        (Some(TypedHeader(Authorization(bearer))), Some(user_id)) => {
            // check for the bearer cache key
            let user_bearer_token = UserBearerToken::try_from(bearer)?;

            let user_redis_key = user_bearer_token.redis_key();

            let mut save_to_redis = false;

            // get the user id that is attached to this bearer token
            let bearer_user_id = match redis_conn.get::<_, u64>(&user_redis_key).await {
                Err(_) => {
                    // TODO: inspect the redis error? if redis is down we should warn
                    // this also means redis being down will not kill our app. Everything will need a db read query though.

                    let user_login = login::Entity::find()
                        .filter(login::Column::BearerToken.eq(user_bearer_token.uuid()))
                        .one(&db_conn)
                        .await
                        .context("database error while querying for user")?
                        .ok_or(FrontendErrorResponse::AccessDenied)?;

                    // check expiration. if expired, delete ALL expired pending_logins
                    let now = Utc::now();

                    if now > user_login.expires_at {
                        // this row is expired! do not allow auth!
                        // delete ALL expired rows.
                        let delete_result = login::Entity::delete_many()
                            .filter(login::Column::ExpiresAt.lte(now))
                            .exec(&db_conn)
                            .await?;

                        // TODO: emit a stat? if this is high something weird might be happening
                        debug!("cleared expired pending_logins: {:?}", delete_result);

                        return Err(FrontendErrorResponse::AccessDenied);
                    }

                    save_to_redis = true;

                    user_login.user_id
                }
                Ok(x) => {
                    // TODO: push cache ttl further in the future?
                    x
                }
            };

            let user_id: u64 = user_id.parse().context("Parsing user_id param")?;

            if bearer_user_id != user_id {
                return Err(FrontendErrorResponse::AccessDenied);
            }

            if save_to_redis {
                // TODO: how long? we store in database for 4 weeks
                let one_day = 60 * 60 * 24;

                if let Err(err) = redis_conn
                    .set_ex::<_, _, ()>(user_redis_key, user_id, one_day)
                    .await
                {
                    warn!("Unable to save user bearer token to redis: {}", err)
                }
            }

            Ok(bearer_user_id)
        }
        (_, None) => {
            // they have a bearer token. we don't care about it on public pages
            // 0 means all
            Ok(0)
        }
        (None, Some(_)) => {
            // they do not have a bearer token, but requested a specific id. block
            // TODO: proper error code from a useful error code
            // TODO: maybe instead of this sharp edged warn, we have a config value?
            // TODO: check config for if we should deny or allow this
            Err(FrontendErrorResponse::AccessDenied)
            // // TODO: make this a flag
            // warn!("allowing without auth during development!");
            // Ok(x.parse()?)
        }
    }
}

/// only allow rpc_key to be set if user_id is also set.
/// this will keep people from reading someone else's keys.
/// 0 means none.

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

pub fn get_query_window_seconds_from_params(
    params: &HashMap<String, String>,
) -> Result<u64, FrontendErrorResponse> {
    params.get("query_window_seconds").map_or_else(
        || {
            // no page in params. set default
            Ok(0)
        },
        |query_window_seconds: &String| {
            // parse the given timestamp
            // TODO: error code 401
            query_window_seconds.parse::<u64>().map_err(|e| {
                FrontendErrorResponse::StatusCode(
                    StatusCode::BAD_REQUEST,
                    "Unable to parse rpc_key_id".to_string(),
                    Some(e.into()),
                )
            })
        },
    )
}

pub fn filter_query_window_seconds(
    params: &HashMap<String, String>,
    response: &mut HashMap<&str, serde_json::Value>,
    q: Select<rpc_accounting::Entity>,
) -> Result<Select<rpc_accounting::Entity>, FrontendErrorResponse> {
    let query_window_seconds = get_query_window_seconds_from_params(params)?;

    if query_window_seconds == 0 {
        // TODO: order by more than this?
        // query_window_seconds is not set so we aggregate all records
        // TODO: i am pretty sure we need to filter by something
        return Ok(q);
    }

    // TODO: is there a better way to do this? how can we get "period_datetime" into this with types?
    // TODO: how can we get the first window to start at query_start_timestamp
    let expr = Expr::cust_with_values(
        "FLOOR(UNIX_TIMESTAMP(rpc_accounting.period_datetime) / ?) * ?",
        [query_window_seconds, query_window_seconds],
    );

    response.insert(
        "query_window_seconds",
        serde_json::Value::Number(query_window_seconds.into()),
    );

    let q = q
        .column_as(expr, "query_window_timestamp")
        .group_by(Expr::cust("query_window_timestamp"))
        // TODO: is there a simpler way to order_by?
        .order_by_asc(SimpleExpr::Custom("query_window_timestamp".to_string()));

    Ok(q)
}

pub enum StatResponse {
    Aggregated,
    Detailed,
}

pub async fn query_user_stats<'a>(
    app: &'a Web3ProxyApp,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    params: &'a HashMap<String, String>,
    stat_response_type: StatResponse,
) -> Result<HashMap<&'a str, serde_json::Value>, FrontendErrorResponse> {
    let db_conn = app.db_conn().context("connecting to db")?;
    let redis_conn = app.redis_conn().await.context("connecting to redis")?;

    let mut response = HashMap::new();

    let q = rpc_accounting::Entity::find()
        .select_only()
        .column_as(
            rpc_accounting::Column::FrontendRequests.sum(),
            "total_frontend_requests",
        )
        .column_as(
            rpc_accounting::Column::BackendRequests.sum(),
            "total_backend_retries",
        )
        .column_as(
            rpc_accounting::Column::CacheMisses.sum(),
            "total_cache_misses",
        )
        .column_as(rpc_accounting::Column::CacheHits.sum(), "total_cache_hits")
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

    // TODO: make this and q mutable and clean up the code below. no need for more `let q`
    let condition = Condition::all();

    let q = if let StatResponse::Detailed = stat_response_type {
        // group by the columns that we use as keys in other places of the code
        q.column(rpc_accounting::Column::ErrorResponse)
            .group_by(rpc_accounting::Column::ErrorResponse)
            .column(rpc_accounting::Column::Method)
            .group_by(rpc_accounting::Column::Method)
            .column(rpc_accounting::Column::ArchiveRequest)
            .group_by(rpc_accounting::Column::ArchiveRequest)
    } else {
        q
    };

    let q = filter_query_window_seconds(params, &mut response, q)?;

    // aggregate stats after query_start
    // TODO: minimum query_start of 90 days?
    let query_start = get_query_start_from_params(params)?;
    // TODO: if no query_start, don't add to response or condition
    response.insert(
        "query_start",
        serde_json::Value::Number(query_start.timestamp().into()),
    );
    let condition = condition.add(rpc_accounting::Column::PeriodDatetime.gte(query_start));

    // filter on chain_id
    let chain_id = get_chain_id_from_params(app, params)?;
    let (condition, q) = if chain_id == 0 {
        // fetch all the chains. don't filter or aggregate
        (condition, q)
    } else {
        let condition = condition.add(rpc_accounting::Column::ChainId.eq(chain_id));

        response.insert("chain_id", serde_json::Value::Number(chain_id.into()));

        (condition, q)
    };

    // get_user_id_from_params checks that the bearer is connected to this user_id
    // TODO: match on user_id and rpc_key_id?
    let user_id = get_user_id_from_params(redis_conn, db_conn.clone(), bearer, params).await?;
    let (condition, q) = if user_id == 0 {
        // 0 means everyone. don't filter on user
        // TODO: 0 or None?
        (condition, q)
    } else {
        let q = q.left_join(rpc_key::Entity);

        let condition = condition.add(rpc_key::Column::UserId.eq(user_id));

        response.insert("user_id", serde_json::Value::Number(user_id.into()));

        (condition, q)
    };

    // filter on rpc_key_id
    // if rpc_key_id, all the requests without a key will be loaded
    // TODO: move getting the param and checking the bearer token into a helper function
    let (condition, q) = if let Some(rpc_key_id) = params.get("rpc_key_id") {
        let rpc_key_id = rpc_key_id.parse::<u64>().map_err(|e| {
            FrontendErrorResponse::StatusCode(
                StatusCode::BAD_REQUEST,
                "Unable to parse rpc_key_id".to_string(),
                Some(e.into()),
            )
        })?;

        response.insert("rpc_key_id", serde_json::Value::Number(rpc_key_id.into()));

        let condition = condition.add(rpc_accounting::Column::RpcKeyId.eq(rpc_key_id));

        let q = q.group_by(rpc_accounting::Column::RpcKeyId);

        if user_id == 0 {
            // no user id, we did not join above
            let q = q.left_join(rpc_key::Entity);

            (condition, q)
        } else {
            // user_id added a join on rpc_key already. only filter on user_id
            let condition = condition.add(rpc_key::Column::UserId.eq(user_id));

            (condition, q)
        }
    } else {
        (condition, q)
    };

    // now that all the conditions are set up. add them to the query
    let q = q.filter(condition);

    // TODO: trace log query here? i think sea orm has a useful log level for this

    // set up pagination
    let page = get_page_from_params(params)?;
    response.insert("page", serde_json::to_value(page).expect("can't fail"));

    // TODO: page size from param with a max from the config
    let page_size = 200;
    response.insert(
        "page_size",
        serde_json::to_value(page_size).expect("can't fail"),
    );

    // query the database
    let query_response = q
        .into_json()
        .paginate(&db_conn, page_size)
        .fetch_page(page)
        // TODO: timeouts here? or are they already set up on the connection
        .await?;

    // add the query_response to the json response
    response.insert("result", serde_json::Value::Array(query_response));

    Ok(response)
}
