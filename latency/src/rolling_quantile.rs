use std::sync::Arc;

use portable_atomic::{AtomicF32, Ordering};
use serde::ser::Serializer;
use serde::Serialize;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use watermill::quantile::RollingQuantile;
use watermill::stats::Univariate;

pub struct RollingQuantileLatency {
    /// Join handle for the latency calculation task.
    pub join_handle: JoinHandle<()>,
    /// Send to update with each request duration.
    latency_tx: flume::Sender<f32>,
    /// rolling quantile latency in seconds. Updated async.
    seconds: Arc<AtomicF32>,
}

/// Task to be spawned per-RollingMedianLatency for calculating the value
#[derive(Debug)]
struct RollingQuantileLatencyTask {
    /// Receive to update each request duration
    latency_rx: flume::Receiver<f32>,
    /// Current estimate and update time
    seconds: Arc<AtomicF32>,
    /// quantile value.
    /// WARNING! should be between 0 and 1.
    q: f32,
    /// Rolling window size.
    window_size: usize,
}

impl RollingQuantileLatencyTask {
    /// Run the loop for updating latency.
    async fn run(self) {
        let mut q = RollingQuantile::new(self.q, self.window_size).unwrap();

        while let Ok(rtt) = self.latency_rx.recv_async().await {
            self.update(&mut q, rtt);
        }
    }

    /// Update the estimate object atomically.
    fn update(&self, q: &mut RollingQuantile<f32>, rtt: f32) {
        q.update(rtt);

        self.seconds
            .store(q.get(), portable_atomic::Ordering::Relaxed);
    }
}

impl RollingQuantileLatency {
    #[inline]
    pub async fn record(&self, duration: Duration) {
        self.record_secs(duration.as_secs_f32()).await
    }

    #[inline]
    pub async fn record_secs(&self, secs: f32) {
        self.latency_tx.send_async(secs).await.unwrap()
    }

    /// Current median.
    #[inline]
    pub fn seconds(&self) -> f32 {
        self.seconds.load(portable_atomic::Ordering::Relaxed)
    }

    /// Current median.
    #[inline]
    pub fn duration(&self) -> Duration {
        Duration::from_secs_f32(self.seconds())
    }
}

impl RollingQuantileLatency {
    pub async fn spawn(quantile_value: f32, window_size: usize) -> Self {
        // TODO: how should queue size and window size be related?
        let (latency_tx, latency_rx) = flume::bounded(window_size);

        let seconds = Arc::new(AtomicF32::new(0.0));

        let task = RollingQuantileLatencyTask {
            latency_rx,
            seconds: seconds.clone(),
            q: quantile_value,
            window_size,
        };

        let join_handle = tokio::spawn(task.run());

        Self {
            join_handle,
            latency_tx,
            seconds,
        }
    }

    #[inline]
    pub async fn spawn_median(window_size: usize) -> Self {
        Self::spawn(0.5, window_size).await
    }
}

/// serialize as seconds
impl Serialize for RollingQuantileLatency {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_f32(self.seconds.load(Ordering::Relaxed))
    }
}
