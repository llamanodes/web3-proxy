use quick_cache::sync::KQCache;
use quick_cache::{PlaceholderGuard, Weighter};
use std::convert::Infallible;
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
    ttl: Duration,
    tx: flume::Sender<(Instant, Key, Qey)>,
    weighter: We,

    pub task_handle: JoinHandle<()>,
}

struct KQCacheWithTTLTask<Key, Qey, Val, We, B> {
    cache: Arc<KQCache<Key, Qey, Val, We, B>>,
    rx: flume::Receiver<(Instant, Key, Qey)>,
}

pub struct PlaceholderGuardWithTTL<'a, Key, Qey, Val, We, B> {
    inner: PlaceholderGuard<'a, Key, Qey, Val, We, B>,
    key: Key,
    qey: Qey,
    ttl: Duration,
    tx: &'a flume::Sender<(Instant, Key, Qey)>,
}

impl<
        Key: Eq + Hash + Clone + Send + Sync + 'static,
        Qey: Eq + Hash + Clone + Send + Sync + 'static,
        Val: Clone + Send + Sync + 'static,
        We: Weighter<Key, Qey, Val> + Clone + Send + Sync + 'static,
        B: BuildHasher + Clone + Send + Sync + 'static,
    > KQCacheWithTTL<Key, Qey, Val, We, B>
{
    pub async fn new_with_options(
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
            rx,
        };

        let task_handle = tokio::spawn(task.run());

        Self {
            cache,
            max_item_weight,
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
        self.cache
            .get_or_insert_async::<Infallible>(key, qey, async move { Ok(f.await) })
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
        self.cache.get_or_insert_async(key, qey, f).await
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
                inner,
                key,
                qey,
                ttl: self.ttl,
                tx: &self.tx,
            }),
        }
    }

    #[inline]
    pub fn hits(&self) -> u64 {
        self.cache.hits()
    }

    /// if the item was too large to insert, it is returned with the error
    #[inline]
    pub fn try_insert(&self, key: Key, qey: Qey, val: Val) -> Result<(), (Key, Qey, Val)> {
        let expire_at = Instant::now() + self.ttl;

        let weight = self.weighter.weight(&key, &qey, &val);

        if weight <= self.max_item_weight {
            self.cache.insert(key.clone(), qey.clone(), val);

            self.tx.send((expire_at, key, qey)).unwrap();

            Ok(())
        } else {
            Err((key, qey, val))
        }
    }

    #[inline]
    pub fn misses(&self) -> u64 {
        self.cache.misses()
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
        Key: Eq + Hash,
        Qey: Eq + Hash,
        Val: Clone,
        We: Weighter<Key, Qey, Val> + Clone,
        B: BuildHasher + Clone,
    > KQCacheWithTTLTask<Key, Qey, Val, We, B>
{
    async fn run(self) {
        while let Ok((expire_at, key, qey)) = self.rx.recv_async().await {
            sleep_until(expire_at).await;

            self.cache.remove(&key, &qey);
        }
    }
}

impl<
        'a,
        Key: Clone + Hash + Eq,
        Qey: Clone + Hash + Eq,
        Val: Clone,
        We: Weighter<Key, Qey, Val>,
        B: BuildHasher,
    > PlaceholderGuardWithTTL<'a, Key, Qey, Val, We, B>
{
    pub fn insert(self, val: Val) {
        let expire_at = Instant::now() + self.ttl;

        self.inner.insert(val);

        self.tx.send((expire_at, self.key, self.qey)).unwrap();
    }
}
