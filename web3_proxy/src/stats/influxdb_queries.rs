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
use ethers::types::{H256, U256};
use ethers::utils::keccak256;
use fstrings::{f, format_args_f};
use hashbrown::HashMap;
use influxdb2::api::query::FluxRecord;
use influxdb2::models::Query;
use log::{debug, error, trace, warn};
use migration::sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use rdkafka::message::ToBytes;
use serde_json::{json, Value};
use ulid::Ulid;

pub async fn query_user_stats<'a>(
    app: &'a Web3ProxyApp,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    params: &'a HashMap<String, String>,
    stat_response_type: StatType,
) -> Web3ProxyResponse {
    // Let's start accumulating the cache-key, if there is any. We will use H256 and keccak, a collision is extremely unlikely
    let mut cache_key: U256 = U256::default();

    let (user_id, _semaphore) = match bearer {
        Some(TypedHeader(Authorization(bearer))) => {
            let (user, semaphore) = app.bearer_is_authorized(bearer).await?;
            (user.id, Some(semaphore))
        }
        None => (0, None),
    };

    // Accumulate the user_id into the cache_key
    cache_key = cache_key
        .overflowing_add(U256::from(keccak256(user_id.to_be_bytes())))
        .0;

    // Return an error if the bearer is **not** set, but the StatType is Detailed
    if stat_response_type == StatType::Detailed && user_id == 0 {
        return Err(Web3ProxyError::BadRequest(
            "Detailed Stats Response requires you to authorize with a bearer token".into(),
        ));
    }

    let db_replica = app
        .db_replica()
        .context("query_user_stats needs a db replica")?;

    // TODO: have a getter for this. do we need a connection pool on it?
    let influxdb_client = app
        .influxdb_client
        .as_ref()
        .context("query_user_stats needs an influxdb client")?;

    let query_window_seconds = get_query_window_seconds_from_params(params)?;
    let query_start = get_query_start_from_params(params)?.timestamp();
    let query_stop = get_query_stop_from_params(params)?.timestamp();
    let chain_id = get_chain_id_from_params(app, params)?;

    if query_start >= query_stop {
        return Err(Web3ProxyError::BadRequest(
            "query_start cannot be after query_stop".into(),
        ));
    }

    cache_key = cache_key
        .overflowing_add(keccak256(query_window_seconds.to_be_bytes()).into())
        .0;
    cache_key = cache_key
        .overflowing_add(keccak256(query_start.to_be_bytes()).into())
        .0;
    cache_key = cache_key
        .overflowing_add(keccak256(query_stop.to_be_bytes()).into())
        .0;
    cache_key = cache_key
        .overflowing_add(keccak256(chain_id.to_be_bytes()).into())
        .0;

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
    cache_key = cache_key
        .overflowing_add(keccak256(measurement.to_bytes()).into())
        .0;

    // Return the cache early, now that we have the full cache-key (if cache entry exists)
    if let Some(cached_response_body) = app.influx_cache.get(&cache_key) {
        debug!("Accessing Cache for Influx Stats!");
        return Ok(Json(json!(cached_response_body)).into_response());
    }

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
                filter_subquery += &f!(r#"r["rpc_secret_key_id"] == "{}""#, user_key);
            } else {
                filter_subquery += &f!(r#"or r["rpc_secret_key_id"] == "{}""#, user_key);
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
        .context("No influxdb bucket was provided")?; // "web3_proxy";

    trace!("Bucket is {:?}", bucket);
    let mut filter_chain_id = "".to_string();
    if chain_id != 0 {
        filter_chain_id = f!(r#"|> filter(fn: (r) => r["chain_id"] == "{chain_id}")"#);
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

    // TODO: I could further also do "and" to every filter, apparently that's faster (according to GPT)
    let drop_method = match stat_response_type {
        StatType::Aggregated => f!(r#"|> drop(columns: ["method"])"#),
        StatType::Detailed => "".to_string(),
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

    let query = f!(r#"
    base = () => from(bucket: "{bucket}")
        |> range(start: {query_start}, stop: {query_stop})
        |> filter(fn: (r) => r["_measurement"] == "{measurement}")
        {filter_chain_id}
        {rpc_key_filter}
        {drop_method}
        
    cumsum = base()
        |> filter(fn: (r) => r["_field"] == "backend_requests" or r["_field"] == "cache_hits" or r["_field"] == "cache_misses" or r["_field"] == "frontend_requests" or r["_field"] == "no_servers" or r["_field"] == "sum_credits_used" or r["_field"] == "sum_request_bytes" or r["_field"] == "sum_response_bytes" or r["_field"] == "sum_response_millis")
        |> group(columns: ["_field", "_measurement", "archive_needed", "chain_id", "error_response", "method", "rpc_secret_key_id"])
        |> aggregateWindow(every: {query_window_seconds}s, fn: sum, createEmpty: false)
        |> drop(columns: ["_start", "_stop"])
        |> pivot(rowKey: ["_time"], columnKey: ["_field"], valueColumn: "_value")
        |> group()
        
    balance = base()
        |> filter(fn: (r) => r["_field"] == "balance")
        |> group(columns: ["_field", "_measurement", "chain_id"])
        |> aggregateWindow(every: {query_window_seconds}s, fn: mean, createEmpty: false)
        |> drop(columns: ["_start", "_stop"])
        |> rename(columns: {{_value: "balance"}})
        |> group()

    join(
        tables: {{cumsum, balance}},
        on: {join_candidates}
    )
    "#);

    debug!("Raw query to db is: {:#?}", query);
    let query = Query::new(query.to_string());
    trace!("Query to db is: {:?}", query);

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
                            out.insert(
                                "rpc_key",
                                serde_json::Value::String(
                                    rpc_key_id_to_key.get(&inner).unwrap().to_string(),
                                ),
                            );
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
                            error!("error_response should always be a Long!");
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
    let mut response_body: HashMap<String, Value> = HashMap::new();
    response_body.insert(
        "num_items".into(),
        serde_json::Value::Number(datapoints.len().into()),
    );
    response_body.insert("result".into(), serde_json::Value::Array(datapoints));
    response_body.insert(
        "query_window_seconds".into(),
        serde_json::Value::Number(query_window_seconds.into()),
    );
    response_body.insert(
        "query_start".into(),
        serde_json::Value::Number(query_start.into()),
    );
    response_body.insert(
        "chain_id".into(),
        serde_json::Value::Number(chain_id.into()),
    );

    if user_id == 0 {
        // 0 means everyone. don't filter on user
    } else {
        response_body.insert("user_id".into(), serde_json::Value::Number(user_id.into()));
    }

    // Also optionally add the rpc_key_id:
    if let Some(rpc_key_id) = params.get("rpc_key_id") {
        let rpc_key_id = rpc_key_id
            .parse::<u64>()
            .map_err(|_| Web3ProxyError::BadRequest("Unable to parse rpc_key_id".into()))?;
        response_body.insert(
            "rpc_key_id".into(),
            serde_json::Value::Number(rpc_key_id.into()),
        );
    }

    let response = Json(json!(response_body)).into_response();

    // TODO: Add a cache for this response (and the corresponding input as well)
    // If we got here, we always want to update the cache (we'll never reach it twice though, unless the cache is too old)
    app.influx_cache.insert(cache_key, response_body).await;

    Ok(response)
}
