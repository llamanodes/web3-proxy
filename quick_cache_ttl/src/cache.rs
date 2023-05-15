use quick_cache::{DefaultHashBuilder, UnitWeighter, Weighter};
use std::{
    future::Future,
    hash::{BuildHasher, Hash},
    time::Duration,
};

use crate::{KQCacheWithTTL, PlaceholderGuardWithTTL};

pub struct CacheWithTTL<Key, Val, We, B>(KQCacheWithTTL<Key, (), Val, We, B>);

impl<Key: Eq + Hash + Clone + Send + Sync + 'static, Val: Clone + Send + Sync + 'static>
    CacheWithTTL<Key, Val, UnitWeighter, DefaultHashBuilder>
{
    pub async fn new_with_unit_weights(estimated_items_capacity: usize, ttl: Duration) -> Self {
        Self::new(
            estimated_items_capacity,
            estimated_items_capacity as u64,
            UnitWeighter,
            DefaultHashBuilder::default(),
            ttl,
        )
        .await
    }
}

impl<
        Key: Eq + Hash + Clone + Send + Sync + 'static,
        Val: Clone + Send + Sync + 'static,
        We: Weighter<Key, (), Val> + Clone + Send + Sync + 'static,
        B: BuildHasher + Clone + Send + Sync + 'static,
    > CacheWithTTL<Key, Val, We, B>
{
    pub async fn new(
        estimated_items_capacity: usize,
        weight_capacity: u64,
        weighter: We,
        hash_builder: B,
        ttl: Duration,
    ) -> Self {
        let inner = KQCacheWithTTL::new(
            estimated_items_capacity,
            weight_capacity,
            weighter,
            hash_builder,
            ttl,
        )
        .await;

        Self(inner)
    }

    #[inline]
    pub async fn get_or_insert_async<E, Fut>(&self, key: &Key, f: Fut) -> Result<Val, E>
    where
        Fut: Future<Output = Result<Val, E>>,
    {
        self.0.get_or_insert_async(key, &(), f).await
    }

    #[inline]
    pub async fn get_value_or_guard_async(
        &self,
        key: Key,
    ) -> Result<Val, PlaceholderGuardWithTTL<'_, Key, (), Val, We, B>> {
        self.0.get_value_or_guard_async(key, ()).await
    }

    #[inline]
    pub fn insert(&self, key: Key, val: Val) {
        self.0.insert(key, (), val)
    }

    #[inline]
    pub fn remove(&self, key: &Key) -> bool {
        self.0.remove(key, &())
    }
}
