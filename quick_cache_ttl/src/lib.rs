mod cache;
mod kq_cache;

pub use cache::CacheWithTTL;
pub use kq_cache::{KQCacheWithTTL, PlaceholderGuardWithTTL};
pub use quick_cache::sync::{Cache, KQCache};
pub use quick_cache::{DefaultHashBuilder, UnitWeighter, Weighter};

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use tokio::task::yield_now;
    use tokio::time;

    use crate::CacheWithTTL;

    #[tokio::test(start_paused = true)]
    async fn test_time_passing() {
        let x = CacheWithTTL::<u32, u32>::new("test", 2, Duration::from_secs(2)).await;

        assert!(x.get(&0).is_none());

        x.try_insert(0, 0).unwrap();

        assert!(x.get(&0).is_some());

        time::advance(Duration::from_secs(1)).await;

        assert!(x.get(&0).is_some());

        time::advance(Duration::from_secs(1)).await;

        // yield so that the expiration code gets a chance to run
        yield_now().await;

        assert!(x.get(&0).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn test_capacity_based_eviction() {
        let x = CacheWithTTL::<u32, ()>::new("test", 1, Duration::from_secs(2)).await;

        assert!(x.get(&0).is_none());

        x.try_insert(0, ()).unwrap();

        assert!(x.get(&0).is_some());

        x.try_insert(1, ()).unwrap();

        assert!(x.get(&1).is_some());
        assert!(x.get(&0).is_none());
    }

    // #[tokio::test(start_paused = true)]
    // async fn test_overweight() {
    //     todo!("wip");
    // }
}
