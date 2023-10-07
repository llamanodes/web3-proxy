use moka::future::{Cache, CacheBuilder};
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::{fmt::Debug, hash::Hash};
use tokio::sync::broadcast;

pub struct DedupedBroadcaster<T>
where
    T: Clone + Debug + Hash + Send + Sync + 'static,
{
    /// subscribe to this to get deduplicated items
    broadcast_filtered_tx: broadcast::Sender<T>,
    cache: Cache<T, ()>,
    total_unfiltered: Arc<AtomicUsize>,
    total_filtered: Arc<AtomicUsize>,
    total_broadcasts: Arc<AtomicUsize>,
}

impl<T> DedupedBroadcaster<T>
where
    T: Clone + Debug + Eq + Hash + PartialEq + Send + Sync + 'static,
{
    pub fn new(capacity: usize, cache_capacity: usize) -> Arc<Self> {
        let (broadcast_filtered_tx, _) = broadcast::channel(capacity);

        let cache = CacheBuilder::new(cache_capacity as u64)
            .time_to_idle(Duration::from_secs(10 * 60))
            .name("DedupedBroadcaster")
            .build();

        let total_unfiltered = Arc::new(AtomicUsize::new(0));
        let total_filtered = Arc::new(AtomicUsize::new(0));
        let total_broadcasts = Arc::new(AtomicUsize::new(0));

        let x = Self {
            broadcast_filtered_tx,
            cache,
            total_broadcasts,
            total_filtered,
            total_unfiltered,
        };

        Arc::new(x)
    }

    /// filter duplicates and send the rest to any subscribers
    /// TODO: change this to be `send` and put a moka cache here instead of lru. then the de-dupe load will be spread across senders
    pub async fn send(&self, item: T) {
        self.total_unfiltered.fetch_add(1, Ordering::Relaxed);

        self.cache
            .get_with(item.clone(), async {
                self.total_filtered.fetch_add(1, Ordering::Relaxed);

                if let Ok(x) = self.broadcast_filtered_tx.send(item) {
                    self.total_broadcasts.fetch_add(x, Ordering::Relaxed);
                }
            })
            .await;
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
        // TODO: what sizes?
        let broadcaster = DedupedBroadcaster::new(10, 10);

        let mut receiver_1 = broadcaster.subscribe();
        let _receiver_2 = broadcaster.subscribe();

        broadcaster.send(1).await;
        broadcaster.send(1).await;
        broadcaster.send(2).await;
        broadcaster.send(1).await;
        broadcaster.send(2).await;
        broadcaster.send(3).await;
        broadcaster.send(3).await;

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
