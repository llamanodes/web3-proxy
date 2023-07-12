use super::StatType;
use crate::errors::Web3ProxyErrorContext;
use crate::{
    app::Web3ProxyApp,
    errors::{Web3ProxyError, Web3ProxyResponse},
    http_params::{
        get_chain_id_from_params, get_query_start_from_params, get_query_stop_from_params,
        get_query_window_seconds_from_params,
    },
};
use anyhow::Context;
use axum::{
    headers::{authorization::Bearer, Authorization},
    response::IntoResponse,
    Json, TypedHeader,
};
use entities::sea_orm_active_enums::Role;
use entities::{rpc_key, secondary_user};
use fstrings::{f, format_args_f};
use hashbrown::HashMap;
use influxdb2::api::query::FluxRecord;
use influxdb2::models::Query;
use migration::sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::json;
use tracing::{debug, error, trace, warn};
use ulid::Ulid;

pub async fn query_user_stats<'a>(
    app: &'a Web3ProxyApp,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    params: &'a HashMap<String, String>,
    stat_response_type: StatType,
) -> Web3ProxyResponse {
    let caller_user = match bearer {
        Some(TypedHeader(Authorization(bearer))) => {
            let user = app.bearer_is_authorized(bearer).await?;

            Some(user)
        }
        None => None,
    };

    // Return an error if the bearer is **not** set, but the StatType is Detailed
    if stat_response_type == StatType::Detailed && caller_user.is_none() {
        return Err(Web3ProxyError::AccessDenied(
            "Detailed Stats Response requires you to authorize with a bearer token".into(),
        ));
    }

    let db_replica = app.db_replica()?;

    // Read the (optional) user-id from the request, this is the logic for subusers
    // If there is no bearer token, this is not allowed
    let user_id: u64 = params
        .get("user_id")
        .and_then(|x| x.parse::<u64>().ok())
        .unwrap_or_else(|| caller_user.as_ref().map(|x| x.id).unwrap_or_default());

    // Only allow stats if the user has an active premium role
    if let Some(caller_user) = &caller_user {
        if user_id != caller_user.id {
            // check that there is at least on rpc-keys owned by the requested user and related to the caller user
            let user_rpc_key_ids: Vec<u64> = rpc_key::Entity::find()
                .filter(rpc_key::Column::UserId.eq(user_id))
                .all(db_replica.as_ref())
                .await?
                .into_iter()
                .map(|x| x.id)
                .collect::<Vec<_>>();

            if secondary_user::Entity::find()
                .filter(secondary_user::Column::UserId.eq(caller_user.id))
                .filter(secondary_user::Column::RpcSecretKeyId.is_in(user_rpc_key_ids))
                .filter(secondary_user::Column::Role.ne(Role::Collaborator))
                .one(db_replica.as_ref())
                .await?
                .is_none()
            {
                return Err(Web3ProxyError::AccessDenied(
                    "Not a subuser of the given user_id".into(),
                ));
            }
        }
    } else if user_id != 0 {
        return Err(Web3ProxyError::AccessDenied(
            "User Stats Response requires you to authorize with a bearer token".into(),
        ));
    }

    let influxdb_client = app.influxdb_client()?;

    let query_window_seconds = get_query_window_seconds_from_params(params)?;
    let query_start = get_query_start_from_params(params)?.timestamp();
    let query_stop = get_query_stop_from_params(params)?.timestamp();
    let chain_id = get_chain_id_from_params(app, params)?;

    // Return a bad request if query_start == query_stop, because then the query is empty basically
    if query_start == query_stop {
        return Err(Web3ProxyError::BadRequest(
            "Start and Stop date cannot be equal. Please specify a (different) start date.".into(),
        ));
    }

    let measurement = if user_id == 0 {
        "global_proxy"
    } else {
        "opt_in_proxy"
    };

    // Include a hashmap to go from rpc_secret_key_id to the rpc_secret_key
    let mut rpc_key_id_to_key = HashMap::new();

    let rpc_key_filter = if user_id == 0 {
        "".to_string()
    } else {
        // Fetch all rpc_secret_key_ids, and filter for these
        let mut user_rpc_keys = rpc_key::Entity::find()
            .filter(rpc_key::Column::UserId.eq(user_id))
            .all(db_replica.as_ref())
            .await
            .web3_context("failed loading user's key")?
            .into_iter()
            .map(|x| {
                let key = x.id.to_string();
                let val = Ulid::from(x.secret_key);
                rpc_key_id_to_key.insert(key.clone(), val);
                key
            })
            .collect::<Vec<_>>();

        // Fetch all rpc_keys where we are the subuser
        let mut subuser_rpc_keys = secondary_user::Entity::find()
            .filter(secondary_user::Column::UserId.eq(user_id))
            .find_also_related(rpc_key::Entity)
            .all(db_replica.as_ref())
            // TODO: Do a join with rpc-keys
            .await
            .web3_context("failed loading subuser keys")?
            .into_iter()
            .flat_map(
                |(subuser, wrapped_shared_rpc_key)| match wrapped_shared_rpc_key {
                    Some(shared_rpc_key) => {
                        if subuser.role == Role::Admin || subuser.role == Role::Owner {
                            let key = shared_rpc_key.id.to_string();
                            let val = Ulid::from(shared_rpc_key.secret_key);
                            rpc_key_id_to_key.insert(key.clone(), val);
                            Some(key)
                        } else {
                            None
                        }
                    }
                    None => None,
                },
            )
            .collect::<Vec<_>>();

        user_rpc_keys.append(&mut subuser_rpc_keys);

        if user_rpc_keys.is_empty() {
            return Err(Web3ProxyError::BadRequest(
                "User has no secret RPC keys yet".into(),
            ));
        }

        // Iterate, pop and add to string
        let mut filter_subquery = "".to_string();

        for (idx, user_key) in user_rpc_keys.iter().enumerate() {
            if idx == 0 {
                filter_subquery += &f!(r#"r.rpc_secret_key_id == "{}""#, user_key);
            } else {
                filter_subquery += &f!(r#"or r.rpc_secret_key_id == "{}""#, user_key);
            }
        }

        f!(r#"|> filter(fn: (r) => {})"#, filter_subquery)
    };

    // TODO: Turn into a 500 error if bucket is not found ..
    // Or just unwrap or so
    let bucket = &app
        .config
        .influxdb_bucket
        .clone()
        .context("No influxdb bucket was provided")?;

    trace!("Bucket is {:?}", bucket);
    let mut filter_chain_id = "".to_string();
    if chain_id != 0 {
        filter_chain_id = f!(r#"|> filter(fn: (r) => r.chain_id == "{chain_id}")"#);
    }

    // Fetch and request for balance

    trace!(
        "Query start and stop are: {:?} {:?}",
        query_start,
        query_stop
    );
    // info!("Query column parameters are: {:?}", stats_column);
    trace!("Query measurement is: {:?}", measurement);
    trace!("Filters are: {:?}", filter_chain_id); // filter_field
    trace!("window seconds are: {:?}", query_window_seconds);

    let group_keys = match stat_response_type {
        StatType::Aggregated => {
            r#"[
            "_field",
            "_measurement",
            "archive_needed",
            "chain_id",
            "error_response",
            "rpc_secret_key_id",
        ]"#
        }
        StatType::Detailed => {
            r#"[
            "_field",
            "_measurement",
            "archive_needed",
            "chain_id",
            "error_response",
            "method",
            "rpc_secret_key_id",
            "user_error_response",
        ]"#
        }
    };

    let join_candidates = f!(
        r#"{:?}"#,
        vec![
            "_time",
            "_measurement",
            "chain_id",
            // "rpc_secret_key_id"
        ]
    );

    let query;
    if stat_response_type == StatType::Detailed
        || (stat_response_type == StatType::Aggregated && user_id != 0)
    {
        query = f!(r#"
            base = () => from(bucket: "{bucket}")
                |> range(start: {query_start}, stop: {query_stop})
                {rpc_key_filter}
                {filter_chain_id}
                |> filter(fn: (r) => r._measurement == "{measurement}")
                
            cumsum = base()
                |> filter(fn: (r) => r._field == "backend_requests" or r._field == "cache_hits" or r._field == "cache_misses" or r._field == "frontend_requests" or r._field == "no_servers" or r._field == "sum_credits_used" or r._field == "sum_request_bytes" or r._field == "sum_response_bytes" or r._field == "sum_response_millis")
                |> group(columns: {group_keys})
                |> aggregateWindow(every: {query_window_seconds}s, fn: sum, createEmpty: false)
                |> drop(columns: ["_start", "_stop"])
                |> pivot(rowKey: ["_time"], columnKey: ["_field"], valueColumn: "_value")
                |> group()
                
            balance = base()
                |> filter(fn: (r) => r["_field"] == "balance")
                |> group(columns: ["_field", "_measurement", "chain_id"])
                |> aggregateWindow(every: {query_window_seconds}s, fn: mean, createEmpty: false)
                |> drop(columns: ["_start", "_stop"])
                |> pivot(rowKey: ["_time"], columnKey: ["_field"], valueColumn: "_value")
                |> group()
        
            join(
                tables: {{cumsum, balance}},
                on: {join_candidates}
            )
        "#);
    } else if stat_response_type == StatType::Aggregated && user_id == 0 {
        query = f!(r#"
            from(bucket: "{bucket}")
                |> range(start: {query_start}, stop: {query_stop})
                {filter_chain_id}
                |> filter(fn: (r) => r._measurement == "{measurement}")
                |> filter(fn: (r) => r._field != "balance")
                |> group(columns: {group_keys})
                |> aggregateWindow(every: {query_window_seconds}s, fn: sum, createEmpty: false)
                |> drop(columns: ["_start", "_stop"])
                |> pivot(rowKey: ["_time"], columnKey: ["_field"], valueColumn: "_value")
                |> group()
        "#);
    } else {
        // In this something with our logic is wrong
        return Err(Web3ProxyError::BadResponse(
            "This edge-case should never occur".into(),
        ));
    }

    // TODO: lower log level
    debug!("Raw query to db is: {:#}", query);
    let query = Query::new(query.to_string());
    trace!(?query, "influx");

    // Make the query and collect all data
    let raw_influx_responses: Vec<FluxRecord> = influxdb_client
        .query_raw(Some(query.clone()))
        .await
        .context(format!(
            "failed parsing query result into a FluxRecord. Query={:?}",
            query
        ))?;

    // Basically rename all items to be "total",
    // calculate number of "archive_needed" and "error_responses" through their boolean representations ...
    // HashMap<String, serde_json::Value>
    // let mut datapoints = HashMap::new();
    // TODO: I must be able to probably zip the balance query...
    let datapoints = raw_influx_responses
        .into_iter()
        .map(|x| x.values)
        .map(|value_map| {
            // Unwrap all relevant numbers
            let mut out: HashMap<&str, serde_json::Value> = HashMap::new();
            value_map.into_iter().for_each(|(key, value)| {
                if key == "_measurement" {
                    match value {
                        influxdb2_structmap::value::Value::String(inner) => {
                            if inner == "opt_in_proxy" {
                                out.insert(
                                    "collection",
                                    serde_json::Value::String("opt-in".to_owned()),
                                );
                            } else if inner == "global_proxy" {
                                out.insert(
                                    "collection",
                                    serde_json::Value::String("global".to_owned()),
                                );
                            } else {
                                warn!("Some datapoints are not part of any _measurement!");
                                out.insert(
                                    "collection",
                                    serde_json::Value::String("unknown".to_owned()),
                                );
                            }
                        }
                        _ => {
                            error!("_measurement should always be a String!");
                        }
                    }
                } else if key == "_stop" {
                    match value {
                        influxdb2_structmap::value::Value::TimeRFC(inner) => {
                            out.insert("stop_time", serde_json::Value::String(inner.to_string()));
                        }
                        _ => {
                            error!("_stop should always be a TimeRFC!");
                        }
                    };
                } else if key == "_time" {
                    match value {
                        influxdb2_structmap::value::Value::TimeRFC(inner) => {
                            out.insert("time", serde_json::Value::String(inner.to_string()));
                        }
                        _ => {
                            error!("_stop should always be a TimeRFC!");
                        }
                    }
                } else if key == "backend_requests" {
                    match value {
                        influxdb2_structmap::value::Value::Long(inner) => {
                            out.insert(
                                "total_backend_requests",
                                serde_json::Value::Number(inner.into()),
                            );
                        }
                        _ => {
                            error!("backend_requests should always be a Long!");
                        }
                    }
                } else if key == "balance" {
                    match value {
                        influxdb2_structmap::value::Value::Double(inner) => {
                            out.insert("balance", json!(f64::from(inner)));
                        }
                        _ => {
                            error!("balance should always be a Double!");
                        }
                    }
                } else if key == "cache_hits" {
                    match value {
                        influxdb2_structmap::value::Value::Long(inner) => {
                            out.insert("total_cache_hits", serde_json::Value::Number(inner.into()));
                        }
                        _ => {
                            error!("cache_hits should always be a Long!");
                        }
                    }
                } else if key == "cache_misses" {
                    match value {
                        influxdb2_structmap::value::Value::Long(inner) => {
                            out.insert(
                                "total_cache_misses",
                                serde_json::Value::Number(inner.into()),
                            );
                        }
                        _ => {
                            error!("cache_misses should always be a Long!");
                        }
                    }
                } else if key == "frontend_requests" {
                    match value {
                        influxdb2_structmap::value::Value::Long(inner) => {
                            out.insert(
                                "total_frontend_requests",
                                serde_json::Value::Number(inner.into()),
                            );
                        }
                        _ => {
                            error!("frontend_requests should always be a Long!");
                        }
                    }
                } else if key == "no_servers" {
                    match value {
                        influxdb2_structmap::value::Value::Long(inner) => {
                            out.insert("no_servers", serde_json::Value::Number(inner.into()));
                        }
                        _ => {
                            error!("no_servers should always be a Long!");
                        }
                    }
                } else if key == "sum_credits_used" {
                    match value {
                        influxdb2_structmap::value::Value::Double(inner) => {
                            out.insert("total_credits_used", json!(f64::from(inner)));
                        }
                        _ => {
                            error!("sum_credits_used should always be a Double!");
                        }
                    }
                } else if key == "sum_request_bytes" {
                    match value {
                        influxdb2_structmap::value::Value::Long(inner) => {
                            out.insert(
                                "total_request_bytes",
                                serde_json::Value::Number(inner.into()),
                            );
                        }
                        _ => {
                            error!("sum_request_bytes should always be a Long!");
                        }
                    }
                } else if key == "sum_response_bytes" {
                    match value {
                        influxdb2_structmap::value::Value::Long(inner) => {
                            out.insert(
                                "total_response_bytes",
                                serde_json::Value::Number(inner.into()),
                            );
                        }
                        _ => {
                            error!("sum_response_bytes should always be a Long!");
                        }
                    }
                } else if key == "rpc_secret_key_id" {
                    match value {
                        influxdb2_structmap::value::Value::String(inner) => {
                            match rpc_key_id_to_key.get(&inner) {
                                Some(x) => {
                                    out.insert("rpc_key", serde_json::Value::String(x.to_string()));
                                }
                                None => {
                                    trace!("rpc_secret_key_id is not included in this query")
                                }
                            }
                        }
                        _ => {
                            error!("rpc_secret_key_id should always be a String!");
                        }
                    }
                } else if key == "sum_response_millis" {
                    match value {
                        influxdb2_structmap::value::Value::Long(inner) => {
                            out.insert(
                                "total_response_millis",
                                serde_json::Value::Number(inner.into()),
                            );
                        }
                        _ => {
                            error!("sum_response_millis should always be a Long!");
                        }
                    }
                }
                // Make this if detailed ...
                else if stat_response_type == StatType::Detailed && key == "method" {
                    match value {
                        influxdb2_structmap::value::Value::String(inner) => {
                            out.insert("method", serde_json::Value::String(inner));
                        }
                        _ => {
                            error!("method should always be a String!");
                        }
                    }
                } else if key == "chain_id" {
                    match value {
                        influxdb2_structmap::value::Value::String(inner) => {
                            out.insert("chain_id", serde_json::Value::String(inner));
                        }
                        _ => {
                            error!("chain_id should always be a String!");
                        }
                    }
                } else if key == "archive_needed" {
                    match value {
                        influxdb2_structmap::value::Value::String(inner) => {
                            out.insert(
                                "archive_needed",
                                if inner == "true" {
                                    serde_json::Value::Bool(true)
                                } else if inner == "false" {
                                    serde_json::Value::Bool(false)
                                } else {
                                    serde_json::Value::String("error".to_owned())
                                },
                            );
                        }
                        _ => {
                            error!("archive_needed should always be a String!");
                        }
                    }
                } else if key == "error_response" {
                    match value {
                        influxdb2_structmap::value::Value::String(inner) => {
                            out.insert(
                                "error_response",
                                if inner == "true" {
                                    serde_json::Value::Bool(true)
                                } else if inner == "false" {
                                    serde_json::Value::Bool(false)
                                } else {
                                    serde_json::Value::String("error".to_owned())
                                },
                            );
                        }
                        _ => {
                            error!("error_response should always be a String!");
                        }
                    }
                } else if stat_response_type == StatType::Detailed && key == "user_error_response" {
                    match value {
                        influxdb2_structmap::value::Value::String(inner) => {
                            out.insert(
                                "user_error_response",
                                if inner == "true" {
                                    serde_json::Value::Bool(true)
                                } else if inner == "false" {
                                    serde_json::Value::Bool(false)
                                } else {
                                    serde_json::Value::String("error".to_owned())
                                },
                            );
                        }
                        _ => {
                            error!("user_error_response should always be a String!");
                        }
                    }
                }
            });

            // datapoints.insert(out.get("time"), out);
            json!(out)
        })
        .collect::<Vec<_>>();

    // I suppose archive requests could be either gathered by default (then summed up), or retrieved on a second go.
    // Same with error responses ..
    let mut response_body = HashMap::new();
    response_body.insert(
        "num_items",
        serde_json::Value::Number(datapoints.len().into()),
    );
    response_body.insert("result", serde_json::Value::Array(datapoints));
    response_body.insert(
        "query_window_seconds",
        serde_json::Value::Number(query_window_seconds.into()),
    );
    response_body.insert("query_start", serde_json::Value::Number(query_start.into()));
    response_body.insert("chain_id", serde_json::Value::Number(chain_id.into()));

    if user_id == 0 {
        // 0 means everyone. don't filter on user
    } else {
        response_body.insert("user_id", serde_json::Value::Number(user_id.into()));
    }

    // Also optionally add the rpc_key_id:
    if let Some(rpc_key_id) = params.get("rpc_key_id") {
        let rpc_key_id = rpc_key_id
            .parse::<u64>()
            .map_err(|_| Web3ProxyError::BadRequest("Unable to parse rpc_key_id".into()))?;
        response_body.insert("rpc_key_id", serde_json::Value::Number(rpc_key_id.into()));
    }

    let response = Json(json!(response_body)).into_response();

    Ok(response)
}
