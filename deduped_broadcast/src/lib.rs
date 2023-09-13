use moka::future::{Cache, CacheBuilder};
use std::sync::Arc;
use std::{
    collections::hash_map::DefaultHasher,
    fmt::Debug,
    hash::{Hash, Hasher},
};
use tokio::sync::{broadcast, mpsc};

struct DedbuedBroadcasterTask<T>
where
    T: Clone + Debug + Hash + Send + Sync + 'static,
{
    unfiltered_rx: mpsc::Receiver<T>,
    broadcast_filtered_tx: Arc<broadcast::Sender<T>>,
    cache: Cache<u64, ()>,
}

pub struct DedupedBroadcaster<T>
where
    T: Clone + Debug + Hash + Send + Sync + 'static,
{
    /// takes in things to broadcast. Can include duplicates, but only one will be sent.
    unfiltered_tx: mpsc::Sender<T>,
    /// subscribe to this to get deduplicated items
    broadcast_filtered_tx: Arc<broadcast::Sender<T>>,
}

impl<T> DedbuedBroadcasterTask<T>
where
    T: Clone + Debug + Hash + Send + Sync + 'static,
{
    /// forward things from input_receiver to output_sender if they aren't in the cache
    async fn run(mut self) {
        while let Some(item) = self.unfiltered_rx.recv().await {
            let mut hasher = DefaultHasher::new();
            item.hash(&mut hasher);
            let hashed = hasher.finish();

            // we don't actually care what the return value is. we just want to send only if the cache is empty
            self.cache
                .get_with(hashed, async {
                    self.broadcast_filtered_tx.send(item).unwrap();
                })
                .await;
        }
    }
}

impl<T> DedupedBroadcaster<T>
where
    T: Clone + Debug + Hash + Send + Sync + 'static,
{
    pub fn new(capacity: usize, cache_capacity: u64) -> Self {
        let (unfiltered_tx, unfiltered_rx) = mpsc::channel::<T>(capacity);
        let (broadcast_filtered_tx, _) = broadcast::channel(capacity);

        let cache = CacheBuilder::new(cache_capacity).build();

        let broadcast_filtered_tx = Arc::new(broadcast_filtered_tx);

        let task = DedbuedBroadcasterTask {
            unfiltered_rx,
            broadcast_filtered_tx: broadcast_filtered_tx.clone(),
            cache,
        };

        // TODO: do something with the handle?
        tokio::task::spawn(task.run());

        Self {
            unfiltered_tx,
            broadcast_filtered_tx,
        }
    }

    pub fn sender(&self) -> &mpsc::Sender<T> {
        &self.unfiltered_tx
    }

    pub fn subscribe(&self) -> broadcast::Receiver<T> {
        self.broadcast_filtered_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::task::yield_now;

    #[tokio::test]
    async fn test_deduped_broadcaster() {
        let broadcaster = DedupedBroadcaster::new(10, 10);

        let mut receiver = broadcaster.subscribe();

        broadcaster.sender().send(1).await.unwrap();
        broadcaster.sender().send(1).await.unwrap();
        broadcaster.sender().send(2).await.unwrap();
        broadcaster.sender().send(1).await.unwrap();
        broadcaster.sender().send(2).await.unwrap();
        broadcaster.sender().send(3).await.unwrap();
        broadcaster.sender().send(3).await.unwrap();

        yield_now().await;

        assert_eq!(receiver.recv().await.unwrap(), 1);
        assert_eq!(receiver.recv().await.unwrap(), 2);
        assert_eq!(receiver.recv().await.unwrap(), 3);
    }
}
