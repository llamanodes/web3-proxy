pub mod linkedlist;

use linkedlist::{LinkedList, NodeSlab};
use std::borrow::Borrow;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash};

#[cfg(feature = "hashbrown")]
use hashbrown::HashMap;

#[cfg(not(feature = "hashbrown"))]
use std::collections::HashMap;

pub struct LinkedHashMap<K, V, S = RandomState> {
    slab: NodeSlab<(K, V)>,
    list: LinkedList,
    map: HashMap<K, usize, S>,
}

impl<K, V> LinkedHashMap<K, V>
where
    K: Hash + Eq + Clone,
{
    #[inline]
    pub fn new() -> LinkedHashMap<K, V> {
        LinkedHashMap {
            slab: NodeSlab::new(),
            list: LinkedList::new(),
            map: HashMap::with_capacity_and_hasher(0, RandomState::default()),
        }
    }

    #[inline]
    pub fn with_capacity(cap: usize) -> LinkedHashMap<K, V> {
        LinkedHashMap {
            slab: NodeSlab::with_capacity(cap),
            list: LinkedList::new(),
            map: HashMap::with_capacity_and_hasher(cap, RandomState::default()),
        }
    }
}

impl<K, V, S> Default for LinkedHashMap<K, V, S>
where
    K: Hash + Eq + Clone,
    S: BuildHasher + Default,
{
    #[inline]
    fn default() -> Self {
        Self::with_hasher(S::default())
    }
}

impl<K, V, S> LinkedHashMap<K, V, S>
where
    K: Hash + Eq + Clone,
    S: BuildHasher,
{
    pub fn with_hasher(hash_builder: S) -> Self {
        LinkedHashMap {
            slab: NodeSlab::new(),
            list: LinkedList::new(),
            map: HashMap::with_hasher(hash_builder),
        }
    }

    pub fn with_capacity_and_hasher(capacity: usize, hash_builder: S) -> Self {
        LinkedHashMap {
            slab: NodeSlab::with_capacity(capacity),
            list: LinkedList::new(),
            map: HashMap::with_capacity_and_hasher(capacity, hash_builder),
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        self.slab.reserve(additional);
        self.map.reserve(additional);
    }

    #[inline]
    pub fn shrink_to_fit(&mut self) {
        self.slab.shrink_to_fit();
        self.map.shrink_to_fit();
    }

    #[inline]
    pub fn clear(&mut self) {
        self.slab.clear();
        self.list = LinkedList::new();
        self.map.clear();
    }

    #[cfg_attr(feature = "inline-more", inline(always))]
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        let index = self.list.push(&mut self.slab, (key.clone(), value));
        let index = self.map.insert(key, index)?;
        let (_, value) = self.list.remove(&mut self.slab, index)?;
        Some(value)
    }

    #[cfg_attr(feature = "inline-more", inline(always))]
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let index = *self.map.get(key)?;
        let (_, value) = self.slab.get(index)?;
        Some(value)
    }

    #[cfg_attr(feature = "inline-more", inline(always))]
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let index = *self.map.get(key)?;
        let (_, value) = self.slab.get_mut(index)?;
        Some(value)
    }

    #[cfg_attr(feature = "inline-more", inline(always))]
    pub fn touch<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let index = *self.map.get(key)?;
        let (_, value) = self.list.touch(&mut self.slab, index)?;
        Some(value)
    }

    #[cfg_attr(feature = "inline-more", inline(always))]
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq,
    {
        let index = self.map.remove(key)?;
        let (_, value) = self.list.remove(&mut self.slab, index)?;
        Some(value)
    }

    #[cfg_attr(feature = "inline-more", inline(always))]
    pub fn pop_front(&mut self) -> Option<(K, V)> {
        let (k, v) = self.list.pop_front(&mut self.slab)?;
        self.map.remove(&k)?;
        Some((k, v))
    }

    #[cfg_attr(feature = "inline-more", inline(always))]
    pub fn pop_last(&mut self) -> Option<(K, V)> {
        let (k, v) = self.list.pop_last(&mut self.slab)?;
        self.map.remove(&k)?;
        Some((k, v))
    }
}

#[test]
fn test_linkedhashmap() {
    #[derive(PartialEq, Eq, Debug)]
    struct Bar(u64);

    let mut map = LinkedHashMap::new();

    map.insert(0, Bar(0));
    map.insert(3, Bar(3));
    map.insert(2, Bar(2));
    map.insert(1, Bar(1));

    assert_eq!(4, map.len());

    assert_eq!(Some(&Bar(2)), map.get(&2));
    assert_eq!(Some(&mut Bar(3)), map.touch(&3));
    assert_eq!(Some((0, Bar(0))), map.pop_front());
    assert_eq!(Some((3, Bar(3))), map.pop_last());
    assert_eq!(Some((1, Bar(1))), map.pop_last());

    assert_eq!(1, map.len());
    assert_eq!(Some(&mut Bar(2)), map.get_mut(&2));

    map.clear();
    assert_eq!(0, map.len());
}
