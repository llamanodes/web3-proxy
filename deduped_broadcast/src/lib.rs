use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::{
    collections::hash_map::DefaultHasher,
    fmt::Debug,
    hash::{Hash, Hasher},
};
use tokio::sync::{broadcast, mpsc};

struct DedupedBroadcasterTask<T>
where
    T: Clone + Debug + Hash + Send + Sync + 'static,
{
    unfiltered_rx: mpsc::Receiver<T>,
    broadcast_filtered_tx: Arc<broadcast::Sender<T>>,
    cache: lru::LruCache<u64, ()>,
    total_unfiltered: Arc<AtomicUsize>,
    total_filtered: Arc<AtomicUsize>,
    total_broadcasts: Arc<AtomicUsize>,
}

pub struct DedupedBroadcaster<T>
where
    T: Clone + Debug + Hash + Send + Sync + 'static,
{
    /// takes in things to broadcast. Can include duplicates, but only one will be sent.
    unfiltered_tx: mpsc::Sender<T>,
    /// subscribe to this to get deduplicated items
    broadcast_filtered_tx: Arc<broadcast::Sender<T>>,
    total_unfiltered: Arc<AtomicUsize>,
    total_filtered: Arc<AtomicUsize>,
    total_broadcasts: Arc<AtomicUsize>,
}

impl<T> DedupedBroadcasterTask<T>
where
    T: Clone + Debug + Hash + Send + Sync + 'static,
{
    /// forward things from input_receiver to output_sender if they aren't in the cache
    async fn run(mut self) {
        while let Some(item) = self.unfiltered_rx.recv().await {
            let mut hasher = DefaultHasher::new();
            item.hash(&mut hasher);
            let hashed = hasher.finish();

            self.total_unfiltered
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            // we don't actually care what the return value is. we just want to send only if the cache is empty
            // TODO: count new vs unique
            self.cache.get_or_insert(hashed, || {
                self.total_filtered
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                if let Ok(x) = self.broadcast_filtered_tx.send(item) {
                    self.total_broadcasts.fetch_add(x, Ordering::Relaxed);
                }
            });
        }
    }
}

impl<T> DedupedBroadcaster<T>
where
    T: Clone + Debug + Hash + Send + Sync + 'static,
{
    pub fn new(capacity: usize, cache_capacity: usize) -> Self {
        let (unfiltered_tx, unfiltered_rx) = mpsc::channel::<T>(capacity);
        let (broadcast_filtered_tx, _) = broadcast::channel(capacity);

        let cache = lru::LruCache::new(cache_capacity.try_into().unwrap());

        let broadcast_filtered_tx = Arc::new(broadcast_filtered_tx);

        let total_unfiltered = Arc::new(AtomicUsize::new(0));
        let total_filtered = Arc::new(AtomicUsize::new(0));
        let total_broadcasts = Arc::new(AtomicUsize::new(0));

        let task = DedupedBroadcasterTask {
            unfiltered_rx,
            broadcast_filtered_tx: broadcast_filtered_tx.clone(),
            cache,
            total_unfiltered: total_unfiltered.clone(),
            total_filtered: total_filtered.clone(),
            total_broadcasts: total_broadcasts.clone(),
        };

        // TODO: do something with the handle?
        tokio::task::spawn(task.run());

        Self {
            unfiltered_tx,
            broadcast_filtered_tx,
            total_unfiltered,
            total_filtered,
            total_broadcasts,
        }
    }

    pub fn sender(&self) -> &mpsc::Sender<T> {
        &self.unfiltered_tx
    }

    pub fn subscribe(&self) -> broadcast::Receiver<T> {
        self.broadcast_filtered_tx.subscribe()
    }
}

impl<T> Debug for DedupedBroadcaster<T>
where
    T: Clone + Debug + Hash + Send + Sync + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DedupedBroadcaster")
            .field(
                "total_unfiltered",
                &self.total_unfiltered.load(Ordering::Relaxed),
            )
            .field(
                "total_filtered",
                &self.total_filtered.load(Ordering::Relaxed),
            )
            .field(
                "total_broadcasts",
                &self.total_broadcasts.load(Ordering::Relaxed),
            )
            .field(
                "subscriptions",
                &self.broadcast_filtered_tx.receiver_count(),
            )
            .finish_non_exhaustive()
    }
}

impl<T> Serialize for DedupedBroadcaster<T>
where
    T: Clone + Debug + Hash + Send + Sync + 'static,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("DedupedBroadcaster", 4)?;

        state.serialize_field(
            "total_unfiltered",
            &self.total_unfiltered.load(Ordering::Relaxed),
        )?;
        state.serialize_field(
            "total_filtered",
            &self.total_filtered.load(Ordering::Relaxed),
        )?;
        state.serialize_field(
            "total_broadcasts",
            &self.total_broadcasts.load(Ordering::Relaxed),
        )?;
        state.serialize_field(
            "subscriptions",
            &self.broadcast_filtered_tx.receiver_count(),
        )?;

        state.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::task::yield_now;

    #[tokio::test]
    async fn test_deduped_broadcaster() {
        let broadcaster = DedupedBroadcaster::new(10, 10);

        let mut receiver_1 = broadcaster.subscribe();
        let _receiver_2 = broadcaster.subscribe();

        broadcaster.sender().send(1).await.unwrap();
        broadcaster.sender().send(1).await.unwrap();
        broadcaster.sender().send(2).await.unwrap();
        broadcaster.sender().send(1).await.unwrap();
        broadcaster.sender().send(2).await.unwrap();
        broadcaster.sender().send(3).await.unwrap();
        broadcaster.sender().send(3).await.unwrap();

        yield_now().await;

        assert_eq!(receiver_1.recv().await.unwrap(), 1);
        assert_eq!(receiver_1.recv().await.unwrap(), 2);
        assert_eq!(receiver_1.recv().await.unwrap(), 3);

        yield_now().await;

        assert_eq!(broadcaster.total_unfiltered.load(Ordering::Relaxed), 7);
        assert_eq!(broadcaster.total_filtered.load(Ordering::Relaxed), 3);
        assert_eq!(broadcaster.total_broadcasts.load(Ordering::Relaxed), 6);
    }
}
