use linkedhashmap::LinkedHashMap;
use std::{
    borrow::Borrow,
    collections::hash_map::RandomState,
    hash::{BuildHasher, Hash},
    mem::size_of_val,
};

// TODO: if values have wildly different sizes, this is good. but if they are all about the same, this could be simpler
pub struct FifoSizedMap<K, V, S = RandomState>
where
    K: Hash + Eq + Clone,
    S: BuildHasher + Default,
{
    /// size limit in bytes for the map
    max_size_bytes: usize,
    /// size limit in bytes for a single item in the map
    max_item_bytes: usize,
    /// FIFO
    map: LinkedHashMap<K, V, S>,
}

impl<K, V, S> FifoSizedMap<K, V, S>
where
    K: Hash + Eq + Clone,
    S: BuildHasher + Default,
{
    pub fn new(max_size_bytes: usize, max_share: usize) -> Self {
        let max_item_bytes = max_size_bytes / max_share;

        Self {
            max_size_bytes,
            max_item_bytes,
            map: Default::default(),
        }
    }
}

impl<K, V, S> Default for FifoSizedMap<K, V, S>
where
    K: Hash + Eq + Clone,
    S: BuildHasher + Default,
{
    fn default() -> Self {
        Self::new(
            // 100 MB default cache
            100_000_000,
            // items cannot take more than 1% of the cache
            100,
        )
    }
}

impl<K, V, S> FifoSizedMap<K, V, S>
where
    K: Hash + Eq + Clone,
    S: BuildHasher + Default,
{
    /// if the size is larger than `self.max_size_bytes`, drop items (first in, first out)
    /// no item is allowed to take more than `1/max_share` of the cache
    pub fn insert(&mut self, key: K, value: V) -> bool {
        // TODO: this might be too naive. not sure how much overhead the object has
        let new_size = size_of_val(&key) + size_of_val(&value);

        // no item is allowed to take more than 1% of the cache
        // TODO: get this from config?
        // TODO: trace logging
        if new_size > self.max_item_bytes {
            return false;
        }

        // drop items until the cache has enough room for the new data
        // TODO: this probably has wildly variable timings
        while size_of_val(&self.map) + new_size > self.max_size_bytes {
            // TODO: this isn't an LRU. it's a "least recently created". does that have a fancy name? should we make it an lru? these caches only live for one block
            self.map.pop_front();
        }

        self.map.insert(key, value);

        true
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
