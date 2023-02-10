use std::collections::BTreeMap;

// show what nodes are used most often
use argh::FromArgs;
use log::trace;
use prettytable::{row, Table};

#[derive(FromArgs, PartialEq, Debug)]
/// Second subcommand.
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
    // tier: u64,
    // backup: bool,
    // block_data_limit: u64,
    requests: u64,
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

        let mut by_tier = BTreeMap::<u64, Vec<_>>::new();
        let mut tier_requests = BTreeMap::<u64, u64>::new();
        let mut total_requests = 0;

        for conn in conns {
            let conn = conn.as_object().unwrap();

            let name = conn
                .get("display_name")
                .unwrap_or_else(|| conn.get("name").unwrap())
                .as_str()
                .unwrap();

            if name.ends_with("http") {
                continue;
            }

            let tier = conn.get("tier").unwrap().as_u64().unwrap();

            // let backup = conn.get("backup").unwrap().as_bool().unwrap();

            // let block_data_limit = conn
            //     .get("block_data_limit")
            //     .unwrap()
            //     .as_u64()
            //     .unwrap_or(u64::MAX);

            let requests = conn.get("total_requests").unwrap().as_u64().unwrap();

            let rpc_data = BackendRpcData {
                name,
                // tier,
                // backup,
                // block_data_limit,
                requests,
            };

            total_requests += rpc_data.requests;

            *tier_requests.entry(tier).or_default() += rpc_data.requests;

            by_tier.entry(tier).or_default().push(rpc_data);
        }

        trace!("tier_requests: {:#?}", tier_requests);
        trace!("by_tier: {:#?}", by_tier);

        let mut table = Table::new();

        table.add_row(row![
            "name",
            "tier",
            "rpc_requests",
            "tier_request_pct",
            "total_pct"
        ]);

        let total_requests = total_requests as f32;

        for (tier, rpcs) in by_tier.iter() {
            let t = (*tier_requests.get(tier).unwrap()) as f32;

            for rpc in rpcs.iter() {
                let tier_request_pct = if t == 0.0 {
                    0.0
                } else {
                    (rpc.requests as f32) / t * 100.0
                };

                let total_request_pct = if total_requests == 0.0 {
                    0.0
                } else {
                    (rpc.requests as f32) / total_requests * 100.0
                };

                table.add_row(row![
                    rpc.name,
                    tier,
                    rpc.requests,
                    tier_request_pct,
                    total_request_pct
                ]);
            }
        }

        table.printstd();

        Ok(())
    }
}
