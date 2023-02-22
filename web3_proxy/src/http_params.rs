use crate::app::DatabaseReplica;
use crate::frontend::errors::FrontendErrorResponse;
use crate::{app::Web3ProxyApp, user_token::UserBearerToken};
use anyhow::Context;
use axum::{
    headers::{authorization::Bearer, Authorization},
    TypedHeader,
};
use chrono::{NaiveDateTime, Utc};
use entities::login;
use hashbrown::HashMap;
use log::{debug, trace, warn};
use migration::sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use redis_rate_limiter::{redis::AsyncCommands, RedisConnection};

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

// TODO: return chrono::Utc instead?
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
            let x = x
                .parse::<i64>()
                .context("parsing start timestamp query param")?;

            let x = NaiveDateTime::from_timestamp_opt(x, 0)
                .context("parsing start timestamp query param")?;

            Ok(x)
        },
    )
}

// TODO: return chrono::Utc instead?
pub fn get_query_stop_from_params(
    params: &HashMap<String, String>,
) -> anyhow::Result<chrono::NaiveDateTime> {
    params.get("query_stop").map_or_else(
        || {
            // no timestamp in params. set default
            let x = chrono::Utc::now();

            Ok(x.naive_utc())
        },
        |x: &String| {
            // parse the given timestamp
            let x = x
                .parse::<i64>()
                .context("parsing stop timestamp query param")?;

            let x = NaiveDateTime::from_timestamp_opt(x, 0)
                .context("parsing stop timestamp query param")?;

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
            query_window_seconds.parse::<u64>().map_err(|err| {
                trace!("Unable to parse rpc_key_id: {:#?}", err);
                FrontendErrorResponse::BadRequest("Unable to parse rpc_key_id".to_string())
            })
        },
    )
}
