use super::StatType;
use crate::http_params::get_stats_column_from_params;
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
use influxdb2::models::Query;
use influxdb2::FromDataPoint;
use itertools::Itertools;
use log::trace;
use serde::Serialize;
use serde_json::{json, Number, Value};

// This type-API is extremely brittle! Make sure that the types conform 1-to-1 as defined here
// https://docs.rs/influxdb2-structmap/0.2.0/src/influxdb2_structmap/value.rs.html#1-98
#[derive(Debug, Default, FromDataPoint, Serialize)]
pub struct AggregatedRpcAccounting {
    chain_id: String,
    _field: String,
    _value: i64,
    _time: DateTime<FixedOffset>,
    error_response: String,
    archive_needed: String,
}

#[derive(Debug, Default, FromDataPoint, Serialize)]
pub struct DetailedRpcAccounting {
    chain_id: String,
    _field: String,
    _value: i64,
    _time: DateTime<FixedOffset>,
    error_response: String,
    archive_needed: String,
    method: String,
}

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
    let stats_column = get_stats_column_from_params(params)?;

    // query_window_seconds must be provided, and should be not 1s (?) by default ..

    // Return a bad request if query_start == query_stop, because then the query is empty basically
    if query_start == query_stop {
        return Err(Web3ProxyError::BadRequest(
            "query_start and query_stop date cannot be equal. Please specify a different range"
                .to_owned(),
        ));
    }

    let measurement = if user_id == 0 {
        "global_proxy"
    } else {
        "opt_in_proxy"
    };

    let bucket = &app
        .config
        .influxdb_bucket
        .clone()
        .context("No influxdb bucket was provided")?;
    trace!("Bucket is {:?}", bucket);

    let mut group_columns = vec![
        "chain_id",
        "_measurement",
        "_field",
        "_measurement",
        "error_response",
        "archive_needed",
    ];
    let mut filter_chain_id = "".to_string();

    // Add to group columns the method, if we want the detailed view as well
    if let StatType::Detailed = stat_response_type {
        group_columns.push("method");
    }

    if chain_id == 0 {
        group_columns.push("chain_id");
    } else {
        filter_chain_id = f!(r#"|> filter(fn: (r) => r["chain_id"] == "{chain_id}")"#);
    }

    let group_columns = serde_json::to_string(&json!(group_columns)).unwrap();

    let group = match stat_response_type {
        StatType::DoNotTrack => {
            return Err(Web3ProxyError::BadRequest(
                "You can't get graphs without tracking enabled.".to_string(),
            ))
        }
        StatType::Aggregated => f!(r#"|> group(columns: {group_columns})"#),
        StatType::Detailed => "".to_string(),
    };

    let filter_field = match stat_response_type {
        StatType::Aggregated => {
            f!(r#"|> filter(fn: (r) => r["_field"] == "{stats_column}")"#)
        }
        // TODO: Detailed should still filter it, but just "group-by" method (call it once per each method ...
        // Or maybe it shouldn't filter it ...
        StatType::Detailed => "".to_string(),
        StatType::DoNotTrack => unimplemented!(),
    };

    trace!("query time range: {:?} - {:?}", query_start, query_stop);
    trace!("stats_column: {:?}", stats_column);
    trace!("measurement: {:?}", measurement);
    trace!("filters: {:?} {:?}", filter_field, filter_chain_id);
    trace!("group: {:?}", group);
    trace!("query_window_seconds: {:?}", query_window_seconds);

    let query = f!(r#"
        from(bucket: "{bucket}")
            |> range(start: {query_start}, stop: {query_stop})
            |> filter(fn: (r) => r["_measurement"] == "{measurement}")
            {filter_field}
            {filter_chain_id}
            {group}
            |> aggregateWindow(every: {query_window_seconds}s, fn: sum, createEmpty: false)
            |> group()
    "#);

    trace!("Raw query to db is: {:?}", query);
    let query = Query::new(query.to_string());
    trace!("Query to db is: {:?}", query);

    // Return a different result based on the query
    let datapoints = match stat_response_type {
        StatType::DoNotTrack => unimplemented!(),
        StatType::Aggregated => {
            let influx_responses: Vec<AggregatedRpcAccounting> = influxdb_client
                .query::<AggregatedRpcAccounting>(Some(query))
                .await?;
            trace!("Influx responses are {:?}", &influx_responses);
            for res in &influx_responses {
                trace!("Resp is: {:?}", res);
            }

            influx_responses
                .into_iter()
                .map(|x| (x._time, x))
                .into_group_map()
                .into_iter()
                .map(|(group, grouped_items)| {
                    trace!("Group is: {:?}", group);

                    // Now put all the fields next to each other
                    // (there will be exactly one field per timestamp, but we want to arrive at a new object)
                    let mut out = HashMap::new();
                    // Could also add a timestamp

                    let mut archive_requests = 0;
                    let mut error_responses = 0;

                    out.insert("method".to_owned(), json!("null"));

                    for x in grouped_items {
                        trace!("Iterating over grouped item {:?}", x);

                        let key = format!("total_{}", x._field).to_string();
                        trace!("Looking at {:?}: {:?}", key, x._value);

                        // Insert it once, and then fix it
                        match out.get_mut(&key) {
                            Some(existing) => {
                                match existing {
                                    Value::Number(old_value) => {
                                        trace!("Old value is {:?}", old_value);
                                        // unwrap will error when someone has too many credits ..
                                        let old_value = old_value.as_i64().unwrap();
                                        *existing = serde_json::Value::Number(Number::from(
                                            old_value + x._value,
                                        ));
                                        trace!("New value is {:?}", existing);
                                    }
                                    _ => {
                                        panic!("Should be nothing but a number")
                                    }
                                };
                            }
                            None => {
                                trace!("Does not exist yet! Insert new!");
                                out.insert(key, serde_json::Value::Number(Number::from(x._value)));
                            }
                        };

                        if !out.contains_key("query_window_timestamp") {
                            out.insert(
                                "query_window_timestamp".to_owned(),
                                // serde_json::Value::Number(x.time.timestamp().into())
                                json!(x._time.timestamp()),
                            );
                        }

                        // Interpret archive needed as a boolean
                        let archive_needed = match x.archive_needed.as_str() {
                            "true" => true,
                            "false" => false,
                            _ => unreachable!(),
                        };
                        let error_response = match x.error_response.as_str() {
                            "true" => true,
                            "false" => false,
                            _ => unreachable!(),
                        };

                        // Add up to archive requests and error responses
                        // TODO: Gotta double check if errors & archive is based on frontend requests, or other metrics
                        if x._field == "frontend_requests" && archive_needed {
                            archive_requests += x._value as u64 // This is the number of requests
                        }
                        if x._field == "frontend_requests" && error_response {
                            error_responses += x._value as u64
                        }
                    }

                    out.insert("archive_request".to_owned(), json!(archive_requests));
                    out.insert("error_response".to_owned(), json!(error_responses));

                    json!(out)
                })
                .collect::<Vec<_>>()
        }
        StatType::Detailed => {
            let influx_responses: Vec<DetailedRpcAccounting> = influxdb_client
                .query::<DetailedRpcAccounting>(Some(query))
                .await?;
            trace!("Influx responses are {:?}", &influx_responses);
            for res in &influx_responses {
                trace!("Resp is: {:?}", res);
            }

            // Group by all fields together ..
            influx_responses
                .into_iter()
                .map(|x| ((x._time, x.method.clone()), x))
                .into_group_map()
                .into_iter()
                .map(|(group, grouped_items)| {
                    // Now put all the fields next to each other
                    // (there will be exactly one field per timestamp, but we want to arrive at a new object)
                    let mut out = HashMap::new();
                    // Could also add a timestamp

                    let mut archive_requests = 0;
                    let mut error_responses = 0;

                    // Should probably move this outside ... (?)
                    let method = group.1;
                    out.insert("method".to_owned(), json!(method));

                    for x in grouped_items {
                        trace!("Iterating over grouped item {:?}", x);

                        let key = format!("total_{}", x._field).to_string();
                        trace!("Looking at {:?}: {:?}", key, x._value);

                        // Insert it once, and then fix it
                        match out.get_mut(&key) {
                            Some(existing) => {
                                match existing {
                                    Value::Number(old_value) => {
                                        trace!("Old value is {:?}", old_value);

                                        // unwrap will error when someone has too many credits ..
                                        let old_value = old_value.as_i64().unwrap();
                                        *existing = serde_json::Value::Number(Number::from(
                                            old_value + x._value,
                                        ));

                                        trace!("New value is {:?}", existing.as_i64());
                                    }
                                    _ => {
                                        panic!("Should be nothing but a number")
                                    }
                                };
                            }
                            None => {
                                trace!("Does not exist yet! Insert new!");
                                out.insert(key, serde_json::Value::Number(Number::from(x._value)));
                            }
                        };

                        if !out.contains_key("query_window_timestamp") {
                            out.insert(
                                "query_window_timestamp".to_owned(),
                                json!(x._time.timestamp()),
                            );
                        }

                        // Interpret archive needed as a boolean
                        let archive_needed = match x.archive_needed.as_str() {
                            "true" => true,
                            "false" => false,
                            _ => {
                                panic!("This should never be!")
                            }
                        };
                        let error_response = match x.error_response.as_str() {
                            "true" => true,
                            "false" => false,
                            _ => {
                                panic!("This should never be!")
                            }
                        };

                        // Add up to archive requests and error responses
                        // TODO: Gotta double check if errors & archive is based on frontend requests, or other metrics
                        if x._field == "frontend_requests" && archive_needed {
                            archive_requests += x._value as i32 // This is the number of requests
                        }
                        if x._field == "frontend_requests" && error_response {
                            error_responses += x._value as i32
                        }
                    }

                    out.insert("archive_request".to_owned(), json!(archive_requests));
                    out.insert("error_response".to_owned(), json!(error_responses));

                    json!(out)
                })
                .collect::<Vec<_>>()
        }
    };

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
