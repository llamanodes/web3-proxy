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
        #[derive(Debug, FromQueryResult)]
        struct SelectResult {
            total_frontend_requests: Decimal,
            // pub total_backend_retries: Decimal,
            // pub total_cache_misses: Decimal,
            total_cache_hits: Decimal,
            total_response_bytes: Decimal,
            total_error_responses: Decimal,
            // pub total_response_millis: Decimal,
            first_period_datetime: DateTimeUtc,
            last_period_datetime: DateTimeUtc,
        }

        let mut q = rpc_accounting::Entity::find()
            .select_only()
            .column_as(
                rpc_accounting::Column::FrontendRequests.sum(),
                "total_frontend_requests",
            )
            // .column_as(
            //     rpc_accounting::Column::BackendRequests.sum(),
            //     "total_backend_retries",
            // )
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
            // .column_as(
            //     rpc_accounting::Column::SumResponseMillis.sum(),
            //     "total_response_millis",
            // )
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

            if u_keys.is_empty() {
                return Err(anyhow::anyhow!("no user keys"));
            }

            let u_key_ids: Vec<_> = u_keys.iter().map(|x| x.id).collect();

            condition = condition.add(rpc_accounting::Column::RpcKeyId.is_in(u_key_ids));
        }

        if let Some(start_timestamp) = self.start_timestamp {
            condition = condition.add(rpc_accounting::Column::PeriodDatetime.gte(start_timestamp))
        }

        if let Some(end_timestamp) = self.end_timestamp {
            condition = condition.add(rpc_accounting::Column::PeriodDatetime.lte(end_timestamp))
        }

        q = q.filter(condition);

        // TODO: make this work without into_json. i think we need to make a struct
        let query_response = q
            .into_model::<SelectResult>()
            .one(db_conn)
            .await?
            .context("no query result")?;

        info!(
            "query_response for chain {:?}: {:#?}",
            self.chain_id, query_response
        );

        // let query_seconds: Decimal = query_response
        //     .last_period_datetime
        //     .signed_duration_since(query_response.first_period_datetime)
        //     .num_seconds()
        //     .into();
        // info!("query seconds: {}", query_seconds);

        Ok(())
    }
}
