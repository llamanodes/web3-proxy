mod rtt_estimate;

use std::sync::Arc;

use kanal::SendError;
use log::{error, info, trace};
use tokio::task::JoinHandle;
use tokio::time::{Duration, Instant};

use self::rtt_estimate::AtomicRttEstimate;
use crate::util::nanos::nanos;

/// Latency calculation using Peak EWMA algorithm
///
/// Updates are done in a separate task to avoid locking or race
/// conditions. Reads may happen on any thread.
#[derive(Debug)]
pub struct PeakEwmaLatency {
    /// Join handle for the latency calculation task
    pub join_handle: JoinHandle<()>,
    /// Send to update with each request duration
    request_tx: kanal::AsyncSender<Duration>,
    /// Latency average and last update time
    rtt_estimate: Arc<AtomicRttEstimate>,
    /// Decay time
    decay_ns: f64,
}

impl PeakEwmaLatency {
    /// Spawn the task for calculating peak request latency
    ///
    /// Returns a handle that can also be used to read the current
    /// average latency.
    pub fn spawn(decay_ns: f64, buf_size: usize, start_latency: Duration) -> Self {
        debug_assert!(decay_ns > 0.0, "decay_ns must be positive");
        let (request_tx, request_rx) = kanal::bounded_async(buf_size);
        let rtt_estimate = Arc::new(AtomicRttEstimate::new(start_latency));
        let task = PeakEwmaLatencyTask {
            request_rx,
            rtt_estimate: rtt_estimate.clone(),
            update_at: Instant::now(),
            decay_ns,
        };
        let join_handle = tokio::spawn(task.run());
        Self {
            join_handle,
            request_tx,
            rtt_estimate,
            decay_ns,
        }
    }

    /// Get the current peak-ewma latency estimate
    pub fn latency(&self) -> Duration {
        let mut estimate = self.rtt_estimate.load();

        let now = Instant::now();
        debug_assert!(
            estimate.update_at <= now,
            "update_at={:?} in the future",
            estimate.update_at,
        );

        // Update the RTT estimate to account for decay since the last update.
        estimate.update(0.0, self.decay_ns, now)
    }

    /// Report latency from a single request
    ///
    /// Should only be called from the Web3Rpc that owns it.
    pub fn report(&self, duration: Duration) {
        match self.request_tx.try_send(duration) {
            Ok(true) => {
                trace!("success");
            }
            Ok(false) => {
                // We don't want to block if the channel is full, just
                // report the error
                error!("Latency report channel full");
                // TODO: could we spawn a new tokio task to report tthis later?
            }
            Err(SendError::Closed) => {
                unreachable!("Owner should keep channel open");
            }
            Err(SendError::ReceiveClosed) => {
                unreachable!("Receiver should keep channel open");
            }
        };
        //.expect("Owner should keep channel open");
    }
}

/// Task to be spawned per-Web3Rpc for calculating the peak request latency
#[derive(Debug)]
struct PeakEwmaLatencyTask {
    /// Receive new request timings for update
    request_rx: kanal::AsyncReceiver<Duration>,
    /// Current estimate and update time
    rtt_estimate: Arc<AtomicRttEstimate>,
    /// Last update time, used for decay calculation
    update_at: Instant,
    /// Decay time
    decay_ns: f64,
}

impl PeakEwmaLatencyTask {
    /// Run the loop for updating latency
    async fn run(mut self) {
        while let Ok(rtt) = self.request_rx.recv().await {
            self.update(rtt);
        }
    }

    /// Update the estimate object atomically.
    fn update(&mut self, rtt: Duration) {
        let rtt = nanos(rtt);

        let now = Instant::now();
        debug_assert!(
            self.update_at <= now,
            "update_at={:?} in the future",
            self.update_at,
        );

        self.rtt_estimate
            .fetch_update(|mut rtt_estimate| rtt_estimate.update(rtt, self.decay_ns, now));
    }
}

#[cfg(test)]
mod tests {
    use tokio::time::{self, Duration};

    use crate::util::nanos::NANOS_PER_MILLI;

    /// The default RTT estimate decays, so that new nodes are considered if the
    /// default RTT is too high.
    #[tokio::test(start_paused = true)]
    async fn default_decay() {
        let estimate =
            super::PeakEwmaLatency::spawn(NANOS_PER_MILLI * 1_000.0, 8, Duration::from_millis(10));
        let load = estimate.latency();
        assert_eq!(load, Duration::from_millis(10));

        time::advance(Duration::from_millis(100)).await;
        let load = estimate.latency();
        assert!(Duration::from_millis(9) < load && load < Duration::from_millis(10));

        time::advance(Duration::from_millis(100)).await;
        let load = estimate.latency();
        assert!(Duration::from_millis(8) < load && load < Duration::from_millis(9));
    }
}
