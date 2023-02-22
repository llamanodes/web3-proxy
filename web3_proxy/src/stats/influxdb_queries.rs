use super::StatType;
use crate::{
    app::Web3ProxyApp,
    frontend::errors::FrontendErrorResponse,
    http_params::{
        get_chain_id_from_params, get_page_from_params, get_query_start_from_params,
        get_query_stop_from_params, get_query_window_seconds_from_params, get_user_id_from_params,
    },
};
use anyhow::Context;
use axum::{
    headers::{authorization::Bearer, Authorization},
    response::Response,
    TypedHeader,
};
use chrono::{DateTime, FixedOffset};
use fstrings::{f, format_args_f};
use hashbrown::HashMap;
use influxdb2::models::Query;
use influxdb2::FromDataPoint;
use serde_json::json;

#[derive(Debug, Default, FromDataPoint)]
pub struct AggregatedRpcAccounting {
    field: String,
    value: f64,
    time: DateTime<FixedOffset>,
}

pub async fn query_user_stats<'a>(
    app: &'a Web3ProxyApp,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    params: &'a HashMap<String, String>,
    stat_response_type: StatType,
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
    let page = get_page_from_params(params)?;

    let measurement = if user_id == 0 {
        "global_proxy"
    } else {
        "opt_in_proxy"
    };

    let bucket = "web3_proxy";

    let mut group_columns = vec!["_measurement", "_field"];
    let mut filter_chain_id = "".to_string();

    if chain_id == 0 {
        group_columns.push("chain_id");
    } else {
        filter_chain_id = f!(r#"|> filter(fn: (r) => r["chain_id"] == "{chain_id}")"#);
    }

    let group_columns = serde_json::to_string(&json!(group_columns)).unwrap();

    let group = match stat_response_type {
        StatType::Aggregated => f!(r#"|> group(columns: {group_columns})"#),
        StatType::Detailed => "".to_string(),
    };

    let filter_field = match stat_response_type {
        StatType::Aggregated => f!(r#"|> filter(fn: (r) => r["_field"] == "frontend_requests")"#),
        StatType::Detailed => "".to_string(),
    };

    let query = f!(r#"
        from(bucket: "{bucket}")
            |> range(start: {query_start}, stop: {query_stop})
            |> filter(fn: (r) => r["_measurement"] == "{measurement}")
            {filter_field}
            {filter_chain_id}
            {group}
            |> aggregateWindow(every: {query_window_seconds}, fn: mean, createEmpty: false)
            |> yield(name: "mean")
    "#);

    let query = Query::new(qs.to_string());

    let res: Vec<AggregatedRpcAccounting> = influxdb_client.query(Some(query)).await?;

    todo!();
}
