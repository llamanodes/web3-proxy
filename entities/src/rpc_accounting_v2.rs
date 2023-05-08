//! `SeaORM` Entity. Generated by sea-orm-codegen 0.10.7

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "rpc_accounting_v2")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: u64,
    pub rpc_key_id: u64,
    pub chain_id: u64,
    pub origin: String,
    pub period_datetime: DateTimeUtc,
    pub method: String,
    pub archive_needed: bool,
    pub error_response: bool,
    pub frontend_requests: u64,
    pub backend_requests: u64,
    pub backend_retries: u64,
    pub no_servers: u64,
    pub cache_misses: u64,
    pub cache_hits: u64,
    pub sum_request_bytes: u64,
    pub sum_response_millis: u64,
    pub sum_response_bytes: u64,
    #[sea_orm(column_type = "Decimal(Some((20, 10)))")]
    pub sum_credits_used: Decimal,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::rpc_key::Entity",
        from = "Column::RpcKeyId",
        to = "super::rpc_key::Column::Id",
        on_update = "NoAction",
        on_delete = "NoAction"
    )]
    RpcKey,
}

impl Related<super::rpc_key::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::RpcKey.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
