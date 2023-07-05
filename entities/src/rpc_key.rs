//! `SeaORM` Entity. Generated by sea-orm-codegen 0.11.3

use super::sea_orm_active_enums::LogLevel;
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "rpc_key")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: u64,
    pub user_id: u64,
    #[sea_orm(column_type = "Binary(BlobSize::Blob(Some(16)))", unique)]
    pub secret_key: Uuid,
    pub description: Option<String>,
    pub private_txs: bool,
    pub active: bool,
    #[sea_orm(column_type = "Text", nullable)]
    pub allowed_ips: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub allowed_origins: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub allowed_referers: Option<String>,
    #[sea_orm(column_type = "Text", nullable)]
    pub allowed_user_agents: Option<String>,
    #[sea_orm(column_type = "Double")]
    pub log_revert_chance: f64,
    pub log_level: LogLevel,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::revert_log::Entity")]
    RevertLog,
    #[sea_orm(has_many = "super::rpc_accounting::Entity")]
    RpcAccounting,
    #[sea_orm(has_many = "super::rpc_accounting_v2::Entity")]
    RpcAccountingV2,
    #[sea_orm(has_many = "super::secondary_user::Entity")]
    SecondaryUser,
    #[sea_orm(
        belongs_to = "super::user::Entity",
        from = "Column::UserId",
        to = "super::user::Column::Id",
        on_update = "NoAction",
        on_delete = "NoAction"
    )]
    User,
}

impl Related<super::revert_log::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::RevertLog.def()
    }
}

impl Related<super::rpc_accounting::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::RpcAccounting.def()
    }
}

impl Related<super::rpc_accounting_v2::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::RpcAccountingV2.def()
    }
}

impl Related<super::secondary_user::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SecondaryUser.def()
    }
}

impl Related<super::user::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::User.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
