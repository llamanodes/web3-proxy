use super::StatType;
use crate::{
    app::Web3ProxyApp,
    frontend::errors::{Web3ProxyError, Web3ProxyResponse},
    http_params::{
        get_chain_id_from_params, get_query_start_from_params, get_query_stop_from_params,
        get_query_window_seconds_from_params, get_user_id_from_params,
    },
};
use anyhow::Context;
use axum::{
    headers::{authorization::Bearer, Authorization},
    response::IntoResponse,
    Json, TypedHeader,
};
use chrono::{DateTime, FixedOffset};
use fstrings::{f, format_args_f};
use hashbrown::HashMap;
use influxdb2::api::query::FluxRecord;
use influxdb2::models::Query;
use influxdb2::FromDataPoint;
use log::{error, info, warn};
use serde::Serialize;
use serde_json::json;

pub async fn query_user_stats<'a>(
    app: &'a Web3ProxyApp,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    params: &'a HashMap<String, String>,
    stat_response_type: StatType,
) -> Web3ProxyResponse {
    let db_conn = app.db_conn().context("query_user_stats needs a db")?;
    let db_replica = app
        .db_replica()
        .context("query_user_stats needs a db replica")?;
    let mut redis_conn = app
        .redis_conn()
        .await
        .context("query_user_stats had a redis connection error")?
        .context("query_user_stats needs a redis")?;

    warn!("Got here: 1");
    // TODO: have a getter for this. do we need a connection pool on it?
    let influxdb_client = app
        .influxdb_client
        .as_ref()
        .context("query_user_stats needs an influxdb client")?;

    // get the user id first. if it is 0, we should use a cache on the app
    let user_id =
        get_user_id_from_params(&mut redis_conn, &db_conn, &db_replica, bearer, params).await?;

    let query_window_seconds = get_query_window_seconds_from_params(params)?;
    let query_start = get_query_start_from_params(params)?.timestamp();
    let query_stop = get_query_stop_from_params(params)?.timestamp();
    let chain_id = get_chain_id_from_params(app, params)?;

    // Return a bad request if query_start == query_stop, because then the query is empty basically
    if query_start == query_stop {
        return Err(Web3ProxyError::BadRequest(
            "Start and Stop date cannot be equal. Please specify a (different) start date."
                .to_owned(),
        ));
    }

    let measurement = if user_id == 0 {
        "global_proxy"
    } else {
        "opt_in_proxy"
    };

    // TODO: Turn into a 500 error if bucket is not found ..
    // Or just unwrap or so
    let bucket = &app
        .config
        .influxdb_bucket
        .clone()
        .context("No influxdb bucket was provided")?; // "web3_proxy";

    info!("Bucket is {:?}", bucket);
    let mut filter_chain_id = "".to_string();
    if chain_id != 0 {
        filter_chain_id = f!(r#"|> filter(fn: (r) => r["chain_id"] == "{chain_id}")"#);
    }

    info!(
        "Query start and stop are: {:?} {:?}",
        query_start, query_stop
    );
    // info!("Query column parameters are: {:?}", stats_column);
    info!("Query measurement is: {:?}", measurement);
    info!("Filters are: {:?}", filter_chain_id); // filter_field
    info!("window seconds are: {:?}", query_window_seconds);

    let drop_method = match stat_response_type {
        StatType::Aggregated => f!(r#"|> drop(columns: ["method"])"#),
        StatType::Detailed => "".to_string(),
    };

    let query = f!(r#"
        from(bucket: "{bucket}")
        |> range(start: {query_start}, stop: {query_stop})
        |> filter(fn: (r) => r["_measurement"] == "{measurement}")
        {filter_chain_id}
        {drop_method}
        |> aggregateWindow(every: {query_window_seconds}s, fn: sum, createEmpty: false)
        |> pivot(rowKey: ["_time"], columnKey: ["_field"], valueColumn: "_value")
        
        |> map(fn: (r) => ({{ r with "archive_needed": if r.archive_needed == "true" then r.frontend_requests else 0}}))
        |> map(fn: (r) => ({{ r with "error_response": if r.error_response == "true" then r.frontend_requests else 0}}))
        
        |> group(columns: ["_time", "_measurement", "chain_id", "method"])
        |> sort(columns: ["frontend_requests"])
        |> cumulativeSum(columns: ["archive_needed", "error_response", "backend_requests", "cache_hits", "cache_misses", "frontend_requests", "sum_credits_used", "sum_request_bytes", "sum_response_bytes", "sum_response_millis"])
        |> sort(columns: ["frontend_requests"], desc: true)
        |> limit(n: 1)
        |> sort(columns: ["_time"], desc: true)
        |> group()
    "#);

    info!("Raw query to db is: {:?}", query);
    let query = Query::new(query.to_string());
    info!("Query to db is: {:?}", query);

    // Make the query and collect all data
    let raw_influx_responses: Vec<FluxRecord> =
        influxdb_client.query_raw(Some(query.clone())).await?;

    // Basically rename all items to be "total",
    // calculate number of "archive_needed" and "error_responses" through their boolean representations ...
    // HashMap<String, serde_json::Value>
    let datapoints: Vec<_> = raw_influx_responses
        .into_iter()
        // .into_values()
        .map(|x| x.values)
        .map(|value_map| {
            // Unwrap all relevant numbers
            // BTreeMap<String, value::Value>
            let mut out: HashMap<String, serde_json::Value> = HashMap::new();

            value_map
                .into_iter()
                .map(|(key, value)| {
                    if key == "_measurement" {
                        match value {
                            influxdb2_structmap::value::Value::String(inner) => {
                                if inner == "opt_in_proxy" {
                                    out.insert(
                                        "collection".to_owned(),
                                        serde_json::Value::String("opt-in".to_owned()),
                                    );
                                } else if inner == "global_proxy" {
                                    out.insert(
                                        "collection".to_owned(),
                                        serde_json::Value::String("global".to_owned()),
                                    );
                                } else {
                                    warn!("Some datapoints are not part of any _measurement!");
                                    out.insert(
                                        "collection".to_owned(),
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
                                out.insert(
                                    "stop_time".to_owned(),
                                    serde_json::Value::String(inner.to_string()),
                                );
                            }
                            _ => {
                                error!("_stop should always be a TimeRFC!");
                            }
                        };
                    } else if key == "_time" {
                        match value {
                            influxdb2_structmap::value::Value::TimeRFC(inner) => {
                                out.insert(
                                    "time".to_owned(),
                                    serde_json::Value::String(inner.to_string()),
                                );
                            }
                            _ => {
                                error!("_stop should always be a TimeRFC!");
                            }
                        }
                    } else if key == "backend_requests" {
                        match value {
                            influxdb2_structmap::value::Value::Long(inner) => {
                                out.insert(
                                    "total_backend_requests".to_owned(),
                                    serde_json::Value::Number(inner.into()),
                                );
                            }
                            _ => {
                                error!("backend_requests should always be a Long!");
                            }
                        }
                    } else if key == "balance" {
                        match value {
                            influxdb2_structmap::value::Value::Long(inner) => {
                                out.insert(
                                    "balance".to_owned(),
                                    serde_json::Value::Number(inner.into()),
                                );
                            }
                            _ => {
                                error!("balance should always be a Long!");
                            }
                        }
                    } else if key == "cache_hits" {
                        match value {
                            influxdb2_structmap::value::Value::Long(inner) => {
                                out.insert(
                                    "total_cache_hits".to_owned(),
                                    serde_json::Value::Number(inner.into()),
                                );
                            }
                            _ => {
                                error!("cache_hits should always be a Long!");
                            }
                        }
                    } else if key == "cache_misses" {
                        match value {
                            influxdb2_structmap::value::Value::Long(inner) => {
                                out.insert(
                                    "total_cache_misses".to_owned(),
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
                                    "total_frontend_requests".to_owned(),
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
                                out.insert(
                                    "no_servers".to_owned(),
                                    serde_json::Value::Number(inner.into()),
                                );
                            }
                            _ => {
                                error!("no_servers should always be a Long!");
                            }
                        }
                    } else if key == "sum_credits_used" {
                        match value {
                            influxdb2_structmap::value::Value::Long(inner) => {
                                out.insert(
                                    "total_credits_used".to_owned(),
                                    serde_json::Value::Number(inner.into()),
                                );
                            }
                            _ => {
                                error!("sum_credits_used should always be a Long!");
                            }
                        }
                    } else if key == "sum_request_bytes" {
                        match value {
                            influxdb2_structmap::value::Value::Long(inner) => {
                                out.insert(
                                    "total_request_bytes".to_owned(),
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
                                    "total_response_bytes".to_owned(),
                                    serde_json::Value::Number(inner.into()),
                                );
                            }
                            _ => {
                                error!("sum_response_bytes should always be a Long!");
                            }
                        }
                    } else if key == "sum_response_millis" {
                        match value {
                            influxdb2_structmap::value::Value::Long(inner) => {
                                out.insert(
                                    "total_response_millis".to_owned(),
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
                                out.insert("method".to_owned(), serde_json::Value::String(inner));
                            }
                            _ => {
                                error!("method should always be a String!");
                            }
                        }
                    } else if key == "chain_id" {
                        match value {
                            influxdb2_structmap::value::Value::String(inner) => {
                                out.insert("chain_id".to_owned(), serde_json::Value::String(inner));
                            }
                            _ => {
                                error!("chain_id should always be a String!");
                            }
                        }
                    } else if key == "archive_needed" {
                        match value {
                            influxdb2_structmap::value::Value::Long(inner) => {
                                out.insert(
                                    "archive_needed".to_owned(),
                                    serde_json::Value::Number(inner.into()),
                                );
                            }
                            _ => {
                                error!("archive_needed should always be a Long!");
                            }
                        }
                    } else if key == "error_response" {
                        match value {
                            influxdb2_structmap::value::Value::Long(inner) => {
                                out.insert(
                                    "error_response".to_owned(),
                                    serde_json::Value::Number(inner.into()),
                                );
                            }
                            _ => {
                                error!("error_response should always be a Long!");
                            }
                        }
                    }
                    ()
                })
                .collect::<Vec<()>>();

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
            .map_err(|_| Web3ProxyError::BadRequest("Unable to parse rpc_key_id".to_string()))?;
        response_body.insert("rpc_key_id", serde_json::Value::Number(rpc_key_id.into()));
    }

    let response = Json(json!(response_body)).into_response();
    // Add the requests back into out

    // TODO: Now impplement the proper response type

    Ok(response)
}
