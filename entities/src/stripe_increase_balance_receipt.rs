//! `SeaORM` Entity. Generated by sea-orm-codegen 0.12.1

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "stripe_increase_balance_receipt")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: u64,
    pub stripe_payment_intend_id: String,
    pub deposit_to_user_id: Option<u64>,
    #[sea_orm(column_type = "Decimal(Some((20, 10)))")]
    pub amount: Decimal,
    pub currency: String,
    pub status: String,
    pub description: Option<String>,
    pub date_created: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::user::Entity",
        from = "Column::DepositToUserId",
        to = "super::user::Column::Id",
        on_update = "NoAction",
        on_delete = "NoAction"
    )]
    User,
}

impl Related<super::user::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::User.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
