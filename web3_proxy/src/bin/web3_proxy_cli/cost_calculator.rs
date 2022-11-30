use std::str::FromStr;

// select all requests for a timeline. sum bandwidth and request count. give `cost / byte` and `cost / request`.
use anyhow::Context;
use argh::FromArgs;
use entities::{rpc_accounting, rpc_key, user};
use ethers::types::Address;
use log::{debug, info};
use migration::{
    sea_orm::{
        self,
        prelude::{DateTimeUtc, Decimal},
        ColumnTrait, DatabaseConnection, EntityTrait, FromQueryResult, QueryFilter, QuerySelect,
    },
    Condition,
};

#[derive(Debug, PartialEq, Eq)]
pub enum TimeFrame {
    Day,
    Month,
}

impl TimeFrame {
    pub fn as_seconds(&self) -> u64 {
        match self {
            Self::Day => 86_400,
            Self::Month => 2_628_000,
        }
    }
}

impl FromStr for TimeFrame {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s.to_lowercase().as_ref() {
            "day" => Ok(Self::Day),
            "month" => Ok(Self::Month),
            _ => Err(anyhow::anyhow!("Invalid string. Should be day or month.")),
        }
    }
}

/// calculate costs
#[derive(FromArgs, PartialEq, Debug, Eq)]
#[argh(subcommand, name = "cost_calculator")]
pub struct CostCalculatorCommand {
    /// dollar cost of running web3-proxy
    #[argh(positional)]
    cost: Decimal,

    #[argh(positional)]
    cost_timeframe: TimeFrame,

    /// the address of the user to check. If none, check all.
    /// TODO: allow checking a single key
    #[argh(option)]
    address: Option<String>,

    /// the chain id to check. If none, check all.
    #[argh(option)]
    chain_id: Option<u64>,
    // TODO: start and end dates?
}

impl CostCalculatorCommand {
    pub async fn main(self, db_conn: &DatabaseConnection) -> anyhow::Result<()> {
        #[derive(Debug, FromQueryResult)]
        struct SelectResult {
            pub total_frontend_requests: Decimal,
            pub total_backend_retries: Decimal,
            pub total_cache_misses: Decimal,
            pub total_cache_hits: Decimal,
            pub total_response_bytes: Decimal,
            pub total_error_responses: Decimal,
            pub total_response_millis: Decimal,
            pub first_period_datetime: DateTimeUtc,
            pub last_period_datetime: DateTimeUtc,
        }

        let q = rpc_accounting::Entity::find()
            .select_only()
            .column_as(
                rpc_accounting::Column::FrontendRequests.sum(),
                "total_frontend_requests",
            )
            .column_as(
                rpc_accounting::Column::BackendRequests.sum(),
                "total_backend_retries",
            )
            .column_as(
                rpc_accounting::Column::CacheMisses.sum(),
                "total_cache_misses",
            )
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

            if u_keys.is_empty() {
                return Err(anyhow::anyhow!("no user keys"));
            }

            let u_key_ids: Vec<_> = u_keys.iter().map(|x| x.id).collect();

            condition = condition.add(rpc_accounting::Column::RpcKeyId.is_in(u_key_ids));
        }

        let q = q.filter(condition);

        // TODO: make this work without into_json. i think we need to make a struct
        let query_response = q
            .into_model::<SelectResult>()
            .one(db_conn)
            .await?
            .context("no query result")?;

        info!("query_response: {:#?}", query_response);

        // todo!("calculate cost/frontend request, cost/response_byte, calculate with a discount for cache hits");

        let cost_seconds: Decimal = self.cost_timeframe.as_seconds().into();

        debug!("cost_seconds: {}", cost_seconds);

        info!("$0.000005 = goal");

        let query_seconds: Decimal = query_response
            .last_period_datetime
            .signed_duration_since(query_response.first_period_datetime)
            .num_seconds()
            .into();
        info!("query seconds: {}", query_seconds);

        let x = costs(
            query_response.total_frontend_requests,
            query_seconds,
            cost_seconds,
            self.cost,
        );
        info!("${} = frontend request cost", x);

        let x = costs(
            query_response.total_frontend_requests - query_response.total_error_responses,
            query_seconds,
            cost_seconds,
            self.cost,
        );
        info!("${} = frontend request cost excluding errors", x);

        let x = costs(
            query_response.total_frontend_requests
                - query_response.total_error_responses
                - query_response.total_cache_hits,
            query_seconds,
            cost_seconds,
            self.cost,
        );
        info!(
            "${} = frontend request cost excluding errors and cache hits",
            x
        );

        let x = costs(
            query_response.total_frontend_requests
                - query_response.total_error_responses
                - (query_response.total_cache_hits / Decimal::from(2)),
            query_seconds,
            cost_seconds,
            self.cost,
        );
        info!(
            "${} = frontend request cost excluding errors and half cache hits",
            x
        );

        let x = costs(
            query_response.total_response_bytes,
            query_seconds,
            cost_seconds,
            self.cost,
        );
        info!("${} = response byte cost", x);

        // TODO: another script that takes these numbers and applies to a single user?

        Ok(())
    }
}

fn costs(
    query_total: Decimal,
    query_seconds: Decimal,
    cost_seconds: Decimal,
    cost: Decimal,
) -> Decimal {
    let requests_per_second = query_total / query_seconds;
    let requests_per_cost_timeframe = requests_per_second * cost_seconds;
    let request_cost_per_query = cost / requests_per_cost_timeframe;

    request_cost_per_query.round_dp(9)
}
