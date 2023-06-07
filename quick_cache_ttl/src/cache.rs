use crate::{KQCacheWithTTL, PlaceholderGuardWithTTL};
use quick_cache::{DefaultHashBuilder, UnitWeighter, Weighter};
use serde::{Serialize, Serializer};
use std::{
    fmt::Debug,
    future::Future,
    hash::{BuildHasher, Hash},
    num::NonZeroU32,
    sync::Arc,
    time::Duration,
};

pub struct CacheWithTTL<Key, Val, We = UnitWeighter, B = DefaultHashBuilder>(
    KQCacheWithTTL<Key, (), Val, We, B>,
);

impl<
        Key: Clone + Debug + Eq + Hash + Send + Sync + 'static,
        Val: Clone + Send + Sync + 'static,
    > CacheWithTTL<Key, Val, UnitWeighter, DefaultHashBuilder>
{
    pub async fn new(name: &'static str, capacity: usize, ttl: Duration) -> Self {
        Self::new_with_options(
            name,
            capacity,
            1.try_into().unwrap(),
            capacity as u64,
            UnitWeighter,
            DefaultHashBuilder::default(),
            ttl,
        )
        .await
    }

    pub async fn arc_with_capacity(
        name: &'static str,
        capacity: usize,
        ttl: Duration,
    ) -> Arc<Self> {
        let x = Self::new(name, capacity, ttl).await;

        Arc::new(x)
    }
}

impl<
        Key: Clone + Debug + Eq + Hash + Send + Sync + 'static,
        Val: Clone + Send + Sync + 'static,
        We: Weighter<Key, (), Val> + Clone + Send + Sync + 'static,
        B: BuildHasher + Clone + Default + Send + Sync + 'static,
    > CacheWithTTL<Key, Val, We, B>
{
    pub async fn new_with_weights(
        name: &'static str,
        estimated_items_capacity: usize,
        max_item_weigth: NonZeroU32,
        weight_capacity: u64,
        weighter: We,
        ttl: Duration,
    ) -> Self {
        let max_item_weight = max_item_weigth.min((weight_capacity as u32).try_into().unwrap());

        let inner = KQCacheWithTTL::new_with_options(
            name,
            estimated_items_capacity,
            max_item_weight,
            weight_capacity,
            weighter,
            B::default(),
            ttl,
        )
        .await;

        Self(inner)
    }

    pub async fn new_with_options(
        name: &'static str,
        estimated_items_capacity: usize,
        max_item_weight: NonZeroU32,
        weight_capacity: u64,
        weighter: We,
        hash_builder: B,
        ttl: Duration,
    ) -> Self {
        let inner = KQCacheWithTTL::new_with_options(
            name,
            estimated_items_capacity,
            max_item_weight,
            weight_capacity,
            weighter,
            hash_builder,
            ttl,
        )
        .await;

        Self(inner)
    }

    #[inline]
    pub fn get(&self, key: &Key) -> Option<Val> {
        self.0.get(key, &())
    }

    #[inline]
    pub async fn get_or_insert_async<Fut>(&self, key: &Key, f: Fut) -> Val
    where
        Fut: Future<Output = Val>,
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
    pub fn peek(&self, key: &Key) -> Option<Val> {
        self.0.peek(key, &())
    }

    #[inline]
    pub fn remove(&self, key: &Key) -> bool {
        self.0.remove(key, &())
    }

    /// if the item was too large to insert, it is returned with the error
    /// IMPORTANT! Inserting the same key multiple times does NOT reset the TTL!
    #[inline]
    pub fn try_insert(&self, key: Key, val: Val) -> Result<(), (Key, Val)> {
        self.0.try_insert(key, (), val).map_err(|(k, _q, v)| (k, v))
    }

    #[inline]
    pub async fn try_get_or_insert_async<E, Fut>(&self, key: &Key, f: Fut) -> Result<Val, E>
    where
        Fut: Future<Output = Result<Val, E>>,
    {
        self.0.try_get_or_insert_async(key, &(), f).await
    }
}

impl<
        Key: Clone + Debug + Eq + Hash + Send + Sync + 'static,
        Val: Clone + Send + Sync + 'static,
        We: Weighter<Key, (), Val> + Clone + Send + Sync + 'static,
        B: BuildHasher + Clone + Send + Sync + 'static,
    > Serialize for CacheWithTTL<Key, Val, We, B>
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}
