use argh::FromArgs;
use ethers::types::U64;
use ordered_float::OrderedFloat;
use prettytable::{row, Table};
use std::{cmp::Reverse, str::FromStr};

#[derive(FromArgs, PartialEq, Debug)]
/// show what nodes are used most often
#[argh(subcommand, name = "popularity_contest")]
pub struct PopularityContestSubCommand {
    #[argh(positional)]
    /// the web3-proxy url
    /// TODO: query multiple and add them together
    rpc: String,
}

#[derive(Debug)]
struct BackendRpcData<'a> {
    name: &'a str,
    tier: u64,
    backup: bool,
    block_data_limit: u64,
    head_block: u64,
    active_requests: u64,
    internal_requests: u64,
    external_requests: u64,
    head_delay_ms: f64,
    peak_latency_ms: f64,
    weighted_latency_ms: f64,
}

impl PopularityContestSubCommand {
    pub async fn main(self) -> anyhow::Result<()> {
        let x: serde_json::Value = reqwest::get(format!("{}/status", self.rpc))
            .await?
            .json()
            .await?;

        let conns = x
            .as_object()
            .unwrap()
            .get("balanced_rpcs")
            .unwrap()
            .as_object()
            .unwrap()
            .get("conns")
            .unwrap()
            .as_array()
            .unwrap();

        let mut highest_block = 0;
        let mut rpc_data = vec![];
        let mut total_external_requests = 0;

        for conn in conns {
            let conn = conn.as_object().unwrap();

            let name = conn
                .get("display_name")
                .unwrap_or_else(|| conn.get("name").unwrap())
                .as_str()
                .unwrap_or("unknown");

            let tier = conn.get("tier").unwrap().as_u64().unwrap();

            let backup = conn.get("backup").unwrap().as_bool().unwrap();

            let block_data_limit = conn
                .get("block_data_limit")
                .and_then(|x| x.as_u64())
                .unwrap_or(u64::MAX);

            let internal_requests = conn
                .get("internal_requests")
                .and_then(|x| x.as_u64())
                .unwrap_or_default();

            let external_requests = conn
                .get("external_requests")
                .and_then(|x| x.as_u64())
                .unwrap_or_default();

            let active_requests = conn
                .get("active_requests")
                .and_then(|x| x.as_u64())
                .unwrap_or_default();

            let head_block = conn
                .get("head_block")
                .and_then(|x| x.get("block"))
                .and_then(|x| x.get("number"))
                .and_then(|x| U64::from_str(x.as_str().unwrap()).ok())
                .map(|x| x.as_u64())
                .unwrap_or_default();

            highest_block = highest_block.max(head_block);

            // TODO: this was moved to an async lock and so serialize can't fetch it
            let head_delay_ms = conn
                .get("head_delay_ms")
                .and_then(|x| x.as_f64())
                .unwrap_or_default();

            let peak_latency_ms = conn
                .get("peak_latency_ms")
                .and_then(|x| x.as_f64())
                .unwrap_or_default();

            let weighted_latency_ms = conn
                .get("weighted_latency_ms")
                .and_then(|x| x.as_f64())
                .unwrap_or_default();

            let x = BackendRpcData {
                name,
                tier,
                backup,
                block_data_limit,
                active_requests,
                internal_requests,
                external_requests,
                head_block,
                head_delay_ms,
                peak_latency_ms,
                weighted_latency_ms,
            };

            total_external_requests += x.external_requests;

            rpc_data.push(x);
        }

        rpc_data.sort_by_key(|x| {
            (
                Reverse(x.external_requests),
                OrderedFloat(x.weighted_latency_ms),
            )
        });

        let mut table = Table::new();

        table.add_row(row![
            "name",
            "external %",
            "external",
            "internal",
            "active",
            "lag",
            "block_data_limit",
            "head_ms",
            "peak_ms",
            "weighted_ms",
            "tier",
        ]);

        for rpc in rpc_data.into_iter() {
            let external_request_pct = if total_external_requests == 0 {
                0.0
            } else {
                (rpc.external_requests as f32) / (total_external_requests as f32) * 100.0
            };

            let block_data_limit = if rpc.block_data_limit == u64::MAX {
                "archive".to_string()
            } else {
                format!("{}", rpc.block_data_limit)
            };

            let tier = if rpc.backup {
                format!("{}B", rpc.tier)
            } else {
                rpc.tier.to_string()
            };

            let lag = highest_block - rpc.head_block;

            table.add_row(row![
                rpc.name,
                format!("{:.3}", external_request_pct),
                rpc.external_requests,
                rpc.internal_requests,
                rpc.active_requests,
                lag,
                block_data_limit,
                format!("{:.3}", rpc.head_delay_ms),
                rpc.peak_latency_ms,
                format!("{:.3}", rpc.weighted_latency_ms),
                tier,
            ]);
        }

        table.printstd();

        Ok(())
    }
}
