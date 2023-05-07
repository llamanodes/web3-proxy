use std::sync::atomic::Ordering;

use tokio::time::{Duration, Instant};

use crate::util::atomic_f32_pair::AtomicF32Pair;

/// Holds the current RTT estimate and the last time this value was updated.
#[derive(Debug)]
pub struct RttEstimate {
    pub update_at: Instant,
    pub rtt: Duration,
}

impl RttEstimate {
    fn new(start_duration: Duration) -> Self {
        Self {
            update_at: Instant::now(),
            rtt: start_duration,
        }
    }

    /// Convert to pair of f32
    fn as_pair(&self, start_time: Instant) -> [f32; 2] {
        let update_at = self
            .update_at
            .saturating_duration_since(start_time)
            .as_secs_f32();
        let rtt = self.rtt.as_secs_f32();
        [update_at, rtt]
    }

    /// Build from pair of f32
    fn from_pair(pair: [f32; 2], start_time: Instant) -> Self {
        let update_at = start_time + Duration::from_secs_f32(pair[0]);
        let rtt = Duration::from_secs_f32(pair[1]);
        Self { update_at, rtt }
    }
}

/// Atomic storage of RttEstimate using AtomicF32Pair
///
/// Start time is needed to (de-)serialize the update_at instance.
#[derive(Debug)]
pub struct AtomicRttEstimate {
    pair: AtomicF32Pair,
    start_time: Instant,
}

impl AtomicRttEstimate {
    /// Creates a new atomic rtt estimate.
    pub fn new(start_duration: Duration) -> Self {
        let estimate = RttEstimate::new(start_duration);
        Self {
            pair: AtomicF32Pair::new(estimate.as_pair(estimate.update_at)),
            start_time: estimate.update_at,
        }
    }

    /// Loads a value from the atomic rtt estimate.
    ///
    /// This method omits the ordering argument since loads may use
    /// slightly stale data to avoid adding additional latency.
    pub fn load(&self) -> RttEstimate {
        RttEstimate::from_pair(self.pair.load(Ordering::Relaxed), self.start_time)
    }

    /// Fetches the value, and applies a function to it that returns an
    /// new rtt. Retrns the new RttEstimate with new update_at.
    ///
    /// Automatically updates the update_at with Instant::now(). This
    /// method omits ordering arguments, defaulting to Relaxed since
    /// all writes are serial and any reads may rely on slightly stale
    /// data.
    pub fn fetch_update<F>(&self, mut f: F) -> RttEstimate
    where
        F: FnMut(RttEstimate) -> Duration,
    {
        let update_at = Instant::now();
        let mut rtt = Duration::ZERO;
        self.pair
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |pair| {
                rtt = f(RttEstimate::from_pair(pair, self.start_time));
                Some(RttEstimate { rtt, update_at }.as_pair(self.start_time))
            })
            .expect("Should never Err");
        RttEstimate { update_at, rtt }
    }
}

#[cfg(test)]
mod tests {
    use tokio::time::{self, Duration, Instant};

    use super::{AtomicRttEstimate, RttEstimate};

    #[test]
    fn test_rtt_estimate_f32_conversions() {
        let rtt = Duration::from_secs(1);
        let expected = RttEstimate::new(rtt);
        let actual =
            RttEstimate::from_pair(expected.as_pair(expected.update_at), expected.update_at);
        assert_eq!(expected.update_at, actual.update_at);
        assert_eq!(expected.rtt, actual.rtt);
    }

    #[tokio::test(start_paused = true)]
    async fn test_atomic_rtt_estimate_load() {
        let rtt = Duration::from_secs(1);
        let estimate = AtomicRttEstimate::new(rtt);
        let actual = estimate.load();
        assert_eq!(Instant::now(), actual.update_at);
        assert_eq!(rtt, actual.rtt);
    }

    #[tokio::test(start_paused = true)]
    async fn test_atomic_rtt_estimate_fetch_update() {
        let start_time = Instant::now();
        let rtt = Duration::from_secs(1);
        let estimate = AtomicRttEstimate::new(rtt);
        time::advance(Duration::from_secs(1)).await;
        let rv = estimate.fetch_update(|value| {
            assert_eq!(start_time, value.update_at);
            assert_eq!(rtt, value.rtt);
            Duration::from_secs(2)
        });
        assert_eq!(start_time + Duration::from_secs(1), rv.update_at);
        assert_eq!(Duration::from_secs(2), rv.rtt);
    }
}
