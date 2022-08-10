use linkedhashmap::LinkedHashMap;
use std::{
    borrow::Borrow,
    collections::hash_map::RandomState,
    hash::{BuildHasher, Hash},
};

pub struct FifoCountMap<K, V, S = RandomState>
where
    K: Hash + Eq + Clone,
    S: BuildHasher + Default,
{
    /// size limit for the map
    max_count: usize,
    /// FIFO
    map: LinkedHashMap<K, V, S>,
}

impl<K, V, S> FifoCountMap<K, V, S>
where
    K: Hash + Eq + Clone,
    S: BuildHasher + Default,
{
    pub fn new(max_count: usize) -> Self {
        Self {
            max_count,
            map: Default::default(),
        }
    }
}

impl<K, V, S> FifoCountMap<K, V, S>
where
    K: Hash + Eq + Clone,
    S: BuildHasher + Default,
{
    /// if the size is larger than `self.max_size_bytes`, drop items (first in, first out)
    /// no item is allowed to take more than `1/max_share` of the cache
    pub fn insert(&mut self, key: K, value: V) {
        // drop items until the cache has enough room for the new data
        // TODO: this probably has wildly variable timings
        if self.map.len() > self.max_count {
            // TODO: this isn't an LRU. it's a "least recently created". does that have a fancy name? should we make it an lru? these caches only live for one block
            self.map.pop_front();
        }

        self.map.insert(key, value);
    }

    /// get an item from the cache, or None
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        self.map.get(key)
    }
}
