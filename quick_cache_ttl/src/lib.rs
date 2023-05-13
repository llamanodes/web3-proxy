use quick_cache::sync::Cache;
use quick_cache::{PlaceholderGuard, Weighter};
use std::hash::{BuildHasher, Hash};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::{sleep_until, Instant};

pub struct QuickCache<Key, Val, We, B> {
    cache: Arc<Cache<Key, Val, We, B>>,
    pub task_handle: JoinHandle<()>,
    ttl: Duration,
    tx: flume::Sender<(Instant, Key)>,
}

// TODO: join handle that
struct QuickCacheTask<Key, Val, We, B> {
    cache: Arc<Cache<Key, Val, We, B>>,
    rx: flume::Receiver<(Instant, Key)>,
}

pub struct Guard<'a, Key, Qey, Val, We, B> {
    inner: PlaceholderGuard<'a, Key, Qey, Val, We, B>,
    key: Key,
    ttl: Duration,
    tx: &'a flume::Sender<(Instant, Key)>,
}

impl<
        'a,
        Key: Clone + Eq + Hash + PartialEq,
        Qey: Eq + Hash,
        Val: Clone,
        We: Weighter<Key, Qey, Val>,
        B: BuildHasher,
    > Guard<'a, Key, Qey, Val, We, B>
{
    pub fn insert(self, val: Val) {
        let expire_at = Instant::now() + self.ttl;

        self.inner.insert(val);

        self.tx.send((expire_at, self.key)).unwrap();
    }
}

impl<
        Key: Clone + Eq + Hash + Send + Sync + 'static,
        Val: Clone + Send + Sync + 'static,
        We: Weighter<Key, (), Val> + Clone + Send + Sync + 'static,
        B: BuildHasher + Clone + Send + Sync + 'static,
    > QuickCache<Key, Val, We, B>
{
    pub async fn spawn(
        estimated_items_capacity: usize,
        weight_capacity: u64,
        weighter: We,
        hash_builder: B,
        ttl: Duration,
    ) -> Self {
        let (tx, rx) = flume::unbounded();

        let cache = Cache::with(
            estimated_items_capacity,
            weight_capacity,
            weighter,
            hash_builder,
        );

        let cache = Arc::new(cache);

        let task = QuickCacheTask {
            cache: cache.clone(),
            rx,
        };

        let task_handle = tokio::spawn(task.run());

        Self {
            cache,
            task_handle,
            ttl,
            tx,
        }
    }

    pub fn insert(&self, key: Key, val: Val) {
        let expire_at = Instant::now() + self.ttl;

        self.cache.insert(key.clone(), val);

        self.tx.send((expire_at, key)).unwrap();
    }

    pub async fn get_value_or_guard_async(
        &self,
        key: Key,
    ) -> Result<Val, Guard<'_, Key, (), Val, We, B>> {
        match self.cache.get_value_or_guard_async(&key).await {
            Ok(x) => Ok(x),
            Err(inner) => Err(Guard {
                inner,
                key,
                ttl: self.ttl,
                tx: &self.tx,
            }),
        }
    }

    pub fn remove(&self, key: &Key) -> bool {
        self.cache.remove(key)
    }
}

impl<Key: Eq + Hash, Val: Clone, We: Weighter<Key, (), Val> + Clone, B: BuildHasher + Clone>
    QuickCacheTask<Key, Val, We, B>
{
    async fn run(self) {
        while let Ok((expire_at, key)) = self.rx.recv_async().await {
            sleep_until(expire_at).await;

            self.cache.remove(&key);
        }
    }
}
