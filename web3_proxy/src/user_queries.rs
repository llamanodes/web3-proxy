use crate::app::DatabaseReplica;
use crate::frontend::errors::FrontendErrorResponse;
use crate::{app::Web3ProxyApp, user_token::UserBearerToken};
use anyhow::Context;
use axum::response::{IntoResponse, Response};
use axum::Json;
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
use redis_rate_limiter::redis;
use redis_rate_limiter::{redis::AsyncCommands, RedisConnection};
use serde_json::json;

/// get the attached address for the given bearer token.
/// First checks redis. Then checks the database.
/// 0 means all users.
/// This authenticates that the bearer is allowed to view this user_id's stats
pub async fn get_user_id_from_params(
    redis_conn: &mut RedisConnection,
    db_conn: &DatabaseConnection,
    db_replica: &DatabaseReplica,
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
                        .one(db_replica.conn())
                        .await
                        .context("database error while querying for user")?
                        .ok_or(FrontendErrorResponse::AccessDenied)?;

                    // if expired, delete ALL expired logins
                    let now = Utc::now();
                    if now > user_login.expires_at {
                        // this row is expired! do not allow auth!
                        // delete ALL expired logins.
                        let delete_result = login::Entity::delete_many()
                            .filter(login::Column::ExpiresAt.lte(now))
                            .exec(db_conn)
                            .await?;

                        // TODO: emit a stat? if this is high something weird might be happening
                        debug!("cleared expired logins: {:?}", delete_result);

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
                const ONE_DAY: usize = 60 * 60 * 24;

                if let Err(err) = redis_conn
                    .set_ex::<_, _, ()>(user_redis_key, user_id, ONE_DAY)
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
    query_window_seconds: u64,
    response: &mut HashMap<&str, serde_json::Value>,
    q: Select<rpc_accounting::Entity>,
) -> Result<Select<rpc_accounting::Entity>, FrontendErrorResponse> {
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
) -> Result<Response, FrontendErrorResponse> {
    let db_conn = app.db_conn().context("query_user_stats needs a db")?;
    let db_replica = app
        .db_replica()
        .context("query_user_stats needs a db replica")?;
    let mut redis_conn = app
        .redis_conn()
        .await
        .context("query_user_stats had a redis connection error")?
        .context("query_user_stats needs a redis")?;

    // get the user id first. if it is 0, we should use a cache on the app
    let user_id =
        get_user_id_from_params(&mut redis_conn, &db_conn, &db_replica, bearer, params).await?;
    // get the query window seconds now so that we can pick a cache with a good TTL
    // TODO: for now though, just do one cache. its easier
    let query_window_seconds = get_query_window_seconds_from_params(params)?;
    let query_start = get_query_start_from_params(params)?;
    let chain_id = get_chain_id_from_params(app, params)?;
    let page = get_page_from_params(params)?;

    let cache_key = if user_id == 0 {
        // TODO: cacheable query_window_seconds from config
        if [60, 600, 3600, 86400, 86400 * 7, 86400 * 30].contains(&query_window_seconds)
            && query_start.timestamp() % (query_window_seconds as i64) == 0
        {
            None
        } else {
            // TODO: is this a good key?
            let redis_cache_key = format!(
                "query_user_stats:{}:{}:{}:{}:{}",
                chain_id, user_id, query_start, query_window_seconds, page,
            );

            let cached_result: Result<(String, u64), _> = redis::pipe()
                .atomic()
                // get the key and its ttl
                .get(&redis_cache_key)
                .ttl(&redis_cache_key)
                // do the query
                .query_async(&mut redis_conn)
                .await;

            // redis being down should not break the stats page!
            if let Ok((body, ttl)) = cached_result {
                let mut response = body.into_response();

                let headers = response.headers_mut();

                headers.insert(
                    "Cache-Control",
                    format!("max-age={}", ttl)
                        .parse()
                        .expect("max-age should always parse"),
                );

                // TODO: emit a stat

                return Ok(response);
            }

            Some(redis_cache_key)
        }
    } else {
        None
    };

    let mut response_body = HashMap::new();

    let mut q = rpc_accounting::Entity::find()
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
            rpc_accounting::Column::ErrorResponse.sum(),
            "total_error_responses",
        )
        .column_as(
            rpc_accounting::Column::SumResponseMillis.sum(),
            "total_response_millis",
        );

    // TODO: make this and q mutable and clean up the code below. no need for more `let q`
    let mut condition = Condition::all();

    if let StatResponse::Detailed = stat_response_type {
        // group by the columns that we use as keys in other places of the code
        q = q
            .column(rpc_accounting::Column::ErrorResponse)
            .group_by(rpc_accounting::Column::ErrorResponse)
            .column(rpc_accounting::Column::Method)
            .group_by(rpc_accounting::Column::Method)
            .column(rpc_accounting::Column::ArchiveRequest)
            .group_by(rpc_accounting::Column::ArchiveRequest);
    }

    // TODO: have q be &mut?
    q = filter_query_window_seconds(query_window_seconds, &mut response_body, q)?;

    // aggregate stats after query_start
    // TODO: maximum query_start of 90 days ago?
    // TODO: if no query_start, don't add to response or condition
    response_body.insert(
        "query_start",
        serde_json::Value::Number(query_start.timestamp().into()),
    );
    condition = condition.add(rpc_accounting::Column::PeriodDatetime.gte(query_start));

    if chain_id == 0 {
        // fetch all the chains
    } else {
        // filter on chain_id
        condition = condition.add(rpc_accounting::Column::ChainId.eq(chain_id));

        response_body.insert("chain_id", serde_json::Value::Number(chain_id.into()));
    }

    if user_id == 0 {
        // 0 means everyone. don't filter on user
    } else {
        q = q.left_join(rpc_key::Entity);

        condition = condition.add(rpc_key::Column::UserId.eq(user_id));

        response_body.insert("user_id", serde_json::Value::Number(user_id.into()));
    }

    // filter on rpc_key_id
    // if rpc_key_id, all the requests without a key will be loaded
    // TODO: move getting the param and checking the bearer token into a helper function
    if let Some(rpc_key_id) = params.get("rpc_key_id") {
        let rpc_key_id = rpc_key_id.parse::<u64>().map_err(|e| {
            FrontendErrorResponse::StatusCode(
                StatusCode::BAD_REQUEST,
                "Unable to parse rpc_key_id".to_string(),
                Some(e.into()),
            )
        })?;

        response_body.insert("rpc_key_id", serde_json::Value::Number(rpc_key_id.into()));

        condition = condition.add(rpc_accounting::Column::RpcKeyId.eq(rpc_key_id));

        q = q.group_by(rpc_accounting::Column::RpcKeyId);

        if user_id == 0 {
            // no user id, we did not join above
            q = q.left_join(rpc_key::Entity);
        } else {
            // user_id added a join on rpc_key already. only filter on user_id
            condition = condition.add(rpc_key::Column::UserId.eq(user_id));
        }
    }

    // now that all the conditions are set up. add them to the query
    q = q.filter(condition);

    // TODO: trace log query here? i think sea orm has a useful log level for this

    // set up pagination
    response_body.insert("page", serde_json::Value::Number(page.into()));

    // TODO: page size from param with a max from the config
    let page_size = 1_000;
    response_body.insert("page_size", serde_json::Value::Number(page_size.into()));

    // query the database for number of items and pages
    let pages_result = q
        .clone()
        .paginate(db_replica.conn(), page_size)
        .num_items_and_pages()
        .await?;

    response_body.insert("num_items", pages_result.number_of_items.into());
    response_body.insert("num_pages", pages_result.number_of_pages.into());

    // query the database (todo: combine with the pages_result query?)
    let query_response = q
        .into_json()
        .paginate(db_replica.conn(), page_size)
        .fetch_page(page)
        .await?;

    // TODO: be a lot smart about caching
    let ttl = 60;

    // add the query_response to the json response
    response_body.insert("result", serde_json::Value::Array(query_response));

    let mut response = Json(&response_body).into_response();

    let headers = response.headers_mut();

    if let Some(cache_key) = cache_key {
        headers.insert(
            "Cache-Control",
            format!("public, max-age={}", ttl)
                .parse()
                .expect("max-age should always parse"),
        );

        let cache_body = json!(response_body).to_string();

        if let Err(err) = redis_conn
            .set_ex::<_, _, ()>(cache_key, cache_body, ttl)
            .await
        {
            warn!("Redis error while caching query_user_stats: {:?}", err);
        }
    } else {
        headers.insert(
            "Cache-Control",
            format!("private, max-age={}", ttl)
                .parse()
                .expect("max-age should always parse"),
        );
    }

    // TODO: Last-Modified header?

    Ok(response)
}
