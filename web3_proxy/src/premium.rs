use crate::errors::Web3ProxyResult;
use anyhow::Context;
use entities::{user, user_tier};
use ethers::prelude::Address;
use migration::sea_orm::{
    self, ActiveModelTrait, ColumnTrait, DatabaseTransaction, EntityTrait, IntoActiveModel,
    QueryFilter,
};
use tracing::info;

pub async fn get_user_and_tier_from_address(
    user_address: &Address,
    txn: &DatabaseTransaction,
) -> Web3ProxyResult<Option<(user::Model, Option<user_tier::Model>)>> {
    let x = user::Entity::find()
        .filter(user::Column::Address.eq(user_address.as_bytes()))
        .find_also_related(user_tier::Entity)
        .one(txn)
        .await?;

    Ok(x)
}

pub async fn get_user_and_tier_from_id(
    user_id: u64,
    txn: &DatabaseTransaction,
) -> Web3ProxyResult<Option<(user::Model, Option<user_tier::Model>)>> {
    let x = user::Entity::find_by_id(user_id)
        .find_also_related(user_tier::Entity)
        .one(txn)
        .await?;

    Ok(x)
}

/// TODO: improve this so that funding an account that has an "unlimited" key is left alone
pub async fn grant_premium_tier(
    user: &user::Model,
    user_tier: Option<&user_tier::Model>,
    txn: &DatabaseTransaction,
) -> Web3ProxyResult<()> {
    if user_tier.is_none() || user_tier.and_then(|x| x.downgrade_tier_id).is_none() {
        if user_tier.map(|x| x.title.as_str()) == Some("Premium") {
            // user is already premium
        } else {
            info!("upgrading {} to Premium", user.id);

            // switch the user to the premium tier
            let new_user_tier = user_tier::Entity::find()
                .filter(user_tier::Column::Title.like("Premium"))
                .one(txn)
                .await?
                .context("premium tier not found")?;

            let mut user = user.clone().into_active_model();

            user.user_tier_id = sea_orm::Set(new_user_tier.id);

            user.save(txn).await?;
        }
    }

    Ok(())
}
