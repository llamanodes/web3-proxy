use log::{log_enabled, trace};
use quick_cache::sync::KQCache;
use quick_cache::{PlaceholderGuard, Weighter};
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use std::convert::Infallible;
use std::fmt::Debug;
use std::future::Future;
use std::hash::{BuildHasher, Hash};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::{sleep_until, Instant};

pub struct KQCacheWithTTL<Key, Qey, Val, We, B> {
    cache: Arc<KQCache<Key, Qey, Val, We, B>>,
    max_item_weight: NonZeroU32,
    name: &'static str,
    ttl: Duration,
    tx: flume::Sender<(Instant, Key, Qey)>,
    weighter: We,

    pub task_handle: JoinHandle<()>,
}

struct KQCacheWithTTLTask<Key, Qey, Val, We, B> {
    cache: Arc<KQCache<Key, Qey, Val, We, B>>,
    name: &'static str,
    rx: flume::Receiver<(Instant, Key, Qey)>,
}

pub struct PlaceholderGuardWithTTL<'a, Key, Qey, Val, We, B> {
    name: &'a str,
    inner: PlaceholderGuard<'a, Key, Qey, Val, We, B>,
    key: Key,
    qey: Qey,
    ttl: Duration,
    tx: &'a flume::Sender<(Instant, Key, Qey)>,
}

impl<
        Key: Clone + Debug + Eq + Hash + Send + Sync + 'static,
        Qey: Clone + Debug + Eq + Hash + Send + Sync + 'static,
        Val: Clone + Send + Sync + 'static,
        We: Weighter<Key, Qey, Val> + Clone + Send + Sync + 'static,
        B: BuildHasher + Clone + Send + Sync + 'static,
    > KQCacheWithTTL<Key, Qey, Val, We, B>
{
    pub async fn new_with_options(
        name: &'static str,
        estimated_items_capacity: usize,
        max_item_weight: NonZeroU32,
        weight_capacity: u64,
        weighter: We,
        hash_builder: B,
        ttl: Duration,
    ) -> Self {
        let (tx, rx) = flume::unbounded();

        let cache = KQCache::with(
            estimated_items_capacity,
            weight_capacity,
            weighter.clone(),
            hash_builder,
        );

        let cache = Arc::new(cache);

        let task = KQCacheWithTTLTask {
            cache: cache.clone(),
            name,
            rx,
        };

        let task_handle = tokio::spawn(task.run());

        Self {
            cache,
            max_item_weight,
            name,
            task_handle,
            ttl,
            tx,
            weighter,
        }
    }

    #[inline]
    pub fn get(&self, key: &Key, qey: &Qey) -> Option<Val> {
        self.cache.get(key, qey)
    }

    #[inline]
    pub async fn get_or_insert_async<Fut>(&self, key: &Key, qey: &Qey, f: Fut) -> Val
    where
        Fut: Future<Output = Val>,
    {
        self.try_get_or_insert_async::<Infallible, _>(key, qey, async move { Ok(f.await) })
            .await
            .expect("infallible")
    }

    #[inline]
    pub async fn try_get_or_insert_async<E, Fut>(
        &self,
        key: &Key,
        qey: &Qey,
        f: Fut,
    ) -> Result<Val, E>
    where
        Fut: Future<Output = Result<Val, E>>,
    {
        self.cache
            .get_or_insert_async(key, qey, async move {
                let x = f.await;

                if x.is_ok() {
                    let expire_at = Instant::now() + self.ttl;

                    trace!(
                        "{}, {:?}, {:?} expiring in {}s",
                        self.name,
                        &key,
                        &qey,
                        expire_at.duration_since(Instant::now()).as_secs_f32()
                    );

                    self.tx.send((expire_at, key.clone(), qey.clone())).unwrap();
                }

                x
            })
            .await
    }

    #[inline]
    pub async fn get_value_or_guard_async(
        &self,
        key: Key,
        qey: Qey,
    ) -> Result<Val, PlaceholderGuardWithTTL<'_, Key, Qey, Val, We, B>> {
        match self.cache.get_value_or_guard_async(&key, &qey).await {
            Ok(x) => Ok(x),
            Err(inner) => Err(PlaceholderGuardWithTTL {
                name: self.name,
                inner,
                key,
                qey,
                ttl: self.ttl,
                tx: &self.tx,
            }),
        }
    }

    /// if the item was too large to insert, it is returned with the error
    /// IMPORTANT! Inserting the same key multiple times does NOT reset the TTL!
    #[inline]
    pub fn try_insert(&self, key: Key, qey: Qey, val: Val) -> Result<(), (Key, Qey, Val)> {
        let expire_at = Instant::now() + self.ttl;

        let weight = self.weighter.weight(&key, &qey, &val);

        if weight <= self.max_item_weight {
            self.cache.insert(key.clone(), qey.clone(), val);

            trace!(
                "{}, {:?}, {:?} expiring in {}s",
                self.name,
                &key,
                &qey,
                expire_at.duration_since(Instant::now()).as_secs_f32()
            );

            self.tx.send((expire_at, key, qey)).unwrap();

            Ok(())
        } else {
            Err((key, qey, val))
        }
    }

    #[inline]
    pub fn peek(&self, key: &Key, qey: &Qey) -> Option<Val> {
        self.cache.peek(key, qey)
    }

    #[inline]
    pub fn remove(&self, key: &Key, qey: &Qey) -> bool {
        self.cache.remove(key, qey)
    }
}

impl<
        Key: Debug + Eq + Hash,
        Qey: Debug + Eq + Hash,
        Val: Clone,
        We: Weighter<Key, Qey, Val> + Clone,
        B: BuildHasher + Clone,
    > KQCacheWithTTLTask<Key, Qey, Val, We, B>
{
    async fn run(self) {
        trace!("watching for expirations on {}", self.name);

        while let Ok((expire_at, key, qey)) = self.rx.recv_async().await {
            let now = Instant::now();
            if expire_at > now {
                if log_enabled!(log::Level::Trace) {
                    trace!(
                        "{}, {:?}, {:?} sleeping for {}ms.",
                        self.name,
                        key,
                        qey,
                        expire_at.duration_since(now).as_millis(),
                    );
                }

                sleep_until(expire_at).await;

                trace!("{}, {:?}, {:?} done sleeping", self.name, key, qey);
            } else {
                trace!("no need to sleep!");
            }

            if self.cache.remove(&key, &qey) {
                trace!("removed {}, {:?}, {:?}", self.name, key, qey);
            } else {
                trace!("empty {}, {:?}, {:?}", self.name, key, qey);
            };
        }

        trace!("watching for expirations on {}", self.name)
    }
}

impl<
        'a,
        Key: Clone + Debug + Hash + Eq,
        Qey: Clone + Debug + Hash + Eq,
        Val: Clone,
        We: Weighter<Key, Qey, Val>,
        B: BuildHasher,
    > PlaceholderGuardWithTTL<'a, Key, Qey, Val, We, B>
{
    pub fn insert(self, val: Val) {
        let expire_at = Instant::now() + self.ttl;

        self.inner.insert(val);

        if log_enabled!(log::Level::Trace) {
            trace!(
                "{}, {:?}, {:?} expiring in {}s",
                self.name,
                self.key,
                self.qey,
                expire_at.duration_since(Instant::now()).as_secs_f32()
            );
        }

        self.tx.send((expire_at, self.key, self.qey)).unwrap();
    }
}

impl<
        Key: Clone + Debug + Eq + Hash + Send + Sync + 'static,
        Qey: Clone + Debug + Eq + Hash + Send + Sync + 'static,
        Val: Clone + Send + Sync + 'static,
        We: Weighter<Key, Qey, Val> + Clone + Send + Sync + 'static,
        B: BuildHasher + Clone + Send + Sync + 'static,
    > Serialize for KQCacheWithTTL<Key, Qey, Val, We, B>
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct(self.name, 5)?;

        state.serialize_field("len", &self.cache.len())?;
        state.serialize_field("weight", &self.cache.weight())?;

        state.serialize_field("capacity", &self.cache.capacity())?;

        state.serialize_field("hits", &self.cache.hits())?;
        state.serialize_field("misses", &self.cache.misses())?;

        state.end()
    }
}
