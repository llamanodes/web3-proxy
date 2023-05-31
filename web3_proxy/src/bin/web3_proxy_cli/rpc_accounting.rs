// select all requests for a timeline. sum bandwidth and request count. give `cost / byte` and `cost / request`.
use anyhow::Context;
use argh::FromArgs;
use entities::{rpc_accounting, rpc_key, user};
use ethers::types::Address;
use log::info;
use migration::{
    sea_orm::{
        self,
        prelude::{DateTimeUtc, Decimal},
        ColumnTrait, DatabaseConnection, EntityTrait, FromQueryResult, QueryFilter, QuerySelect,
    },
    Condition,
};
use serde::Serialize;
use serde_json::json;

/// count requests
#[derive(FromArgs, PartialEq, Debug, Eq)]
#[argh(subcommand, name = "rpc_accounting")]
pub struct RpcAccountingSubCommand {
    /// the address of the user to check. If none, check all.
    /// TODO: allow checking a single key
    #[argh(option)]
    address: Option<String>,

    /// the chain id to check. If none, check all.
    #[argh(option)]
    chain_id: Option<u64>,

    /// unix epoch timestamp of earliest entry
    #[argh(option)]
    start_timestamp: Option<u64>,

    /// unix epoch timestamp of last entry
    #[argh(option)]
    end_timestamp: Option<u64>,
}

impl RpcAccountingSubCommand {
    pub async fn main(self, db_conn: &DatabaseConnection) -> anyhow::Result<()> {
        #[derive(Serialize, FromQueryResult)]
        struct SelectResult {
            total_frontend_requests: Decimal,
            total_backend_retries: Decimal,
            // total_cache_misses: Decimal,
            total_cache_hits: Decimal,
            total_response_bytes: Decimal,
            total_error_responses: Decimal,
            total_response_millis: Decimal,
            first_period_datetime: DateTimeUtc,
            last_period_datetime: DateTimeUtc,
        }

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
            // .column_as(
            //     rpc_accounting::Column::CacheMisses.sum(),
            //     "total_cache_misses",
            // )
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
            )
            .column_as(
                rpc_accounting::Column::PeriodDatetime.min(),
                "first_period_datetime",
            )
            .column_as(
                rpc_accounting::Column::PeriodDatetime.max(),
                "last_period_datetime",
            );

        let mut condition = Condition::all();

        // TODO: do this with add_option? try operator is harder to use then
        if let Some(address) = self.address {
            let address: Vec<u8> = address.parse::<Address>()?.to_fixed_bytes().into();

            // TODO: find_with_related
            let u = user::Entity::find()
                .filter(user::Column::Address.eq(address))
                .one(db_conn)
                .await?
                .context("no user found")?;

            // TODO: select_only
            let u_keys = rpc_key::Entity::find()
                .filter(rpc_key::Column::UserId.eq(u.id))
                .all(db_conn)
                .await?;

            anyhow::ensure!(!u_keys.is_empty(), "no user keys");

            let u_key_ids: Vec<_> = u_keys.into_iter().map(|x| x.id).collect();

            condition = condition.add(rpc_accounting::Column::RpcKeyId.is_in(u_key_ids));
        }

        if let Some(start_timestamp) = self.start_timestamp {
            condition = condition.add(rpc_accounting::Column::PeriodDatetime.gte(start_timestamp))
        }

        if let Some(end_timestamp) = self.end_timestamp {
            condition = condition.add(rpc_accounting::Column::PeriodDatetime.lte(end_timestamp))
        }

        if let Some(chain_id) = self.chain_id {
            condition = condition.add(rpc_accounting::Column::ChainId.eq(chain_id))
        }

        q = q.filter(condition);

        let stats = q
            .into_model::<SelectResult>()
            .one(db_conn)
            .await?
            .context("no query result")?;

        if let Some(chain_id) = self.chain_id {
            info!("stats for chain {}", chain_id);
        } else {
            info!("stats for all chains");
        }

        info!("stats: {:#}", json!(&stats));

        let query_seconds: Decimal = stats
            .last_period_datetime
            .signed_duration_since(stats.first_period_datetime)
            .num_seconds()
            .into();
        dbg!(query_seconds);

        let avg_request_per_second = (stats.total_frontend_requests / query_seconds).round_dp(2);
        dbg!(avg_request_per_second);

        let cache_hit_rate = (stats.total_cache_hits / stats.total_frontend_requests
            * Decimal::from(100))
        .round_dp(2);
        dbg!(cache_hit_rate);

        let avg_response_millis =
            (stats.total_response_millis / stats.total_frontend_requests).round_dp(3);
        dbg!(avg_response_millis);

        let avg_response_bytes =
            (stats.total_response_bytes / stats.total_frontend_requests).round();
        dbg!(avg_response_bytes);

        Ok(())
    }
}
