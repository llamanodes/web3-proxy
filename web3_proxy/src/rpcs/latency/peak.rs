use log::trace;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Duration, Instant};

/// Latency calculation using Peak EWMA algorithm
///
/// Updates are done in a separate task to avoid locking or race
/// conditions. Reads may happen on any thread.
#[derive(Debug)]
pub struct PeakLatency {
    /// Join handle for the latency calculation task
    pub join_handle: JoinHandle<()>,
    /// Send to update with each request duration
    request_tx: mpsc::Sender<Duration>,
    /// Receive new latency average
    pub latency_rx: watch::Receiver<Duration>,
}

impl PeakLatency {
    /// Spawn the task for calculating peak request latency
    ///
    /// Returns a handle that can also be used to read the current
    /// average latency.
    pub fn spawn(decay_ns: f64) -> PeakLatency {
        debug_assert!(decay_ns > 0.0, "decay_ns must be positive");
        // TODO: is this the correct buffer size?
        let (request_tx, request_rx) = mpsc::channel(1);
        // TODO: is one second a good default here?
        let (latency_tx, latency_rx) = watch::channel(Duration::from_secs(1));
        let task = PeakLatencyTask {
            request_rx,
            latency_tx,
            update_at: Instant::now(),
            decay_ns,
        };
        let join_handle = tokio::spawn(task.run());
        PeakLatency {
            join_handle,
            request_tx,
            latency_rx,
        }
    }

    /// Report latency from a single request
    ///
    /// Should only be called from the Web3Rpc that owns it.
    pub async fn report(&self, duration: Duration) {
        self.request_tx
            .send(duration)
            .await
            .expect("Owner should keep channel open");
    }
}

/// Task to be spawned per-Web3Rpc for calculating the peak request latency
#[derive(Debug)]
struct PeakLatencyTask {
    request_rx: mpsc::Receiver<Duration>,
    latency_tx: watch::Sender<Duration>,
    /// Last update time, used for decay calculation
    update_at: Instant,
    decay_ns: f64,
}

impl PeakLatencyTask {
    /// Run the loop for updating latency
    async fn run(mut self) {
        while let Some(rtt) = self.request_rx.recv().await {
            self.update(rtt);
        }
    }

    fn update(&mut self, rtt: Duration) {
        let rtt = nanos(rtt);

        let now = Instant::now();
        debug_assert!(
            self.update_at <= now,
            "update_at={:?} in the future",
            self.update_at,
        );

        self.latency_tx.send_if_modified(|latency| {
            let ewma = nanos(*latency);
            if ewma < rtt {
                // For Peak-EWMA, always use the worst-case (peak) value as the estimate for
                // subsequent requests.
                trace!(
                    "update peak rtt={}ms prior={}ms",
                    rtt / NANOS_PER_MILLI,
                    ewma / NANOS_PER_MILLI,
                );
                false
            } else {
                // When a latency is observed that is less than the estimated latency, we decay the
                // prior estimate according to how much time has elapsed since the last
                // update. The inverse of the decay is used to scale the estimate towards the
                // observed latency value.
                let elapsed = nanos(now.saturating_duration_since(self.update_at));
                let decay = (-elapsed / self.decay_ns).exp();
                let recency = 1.0 - decay;
                let next_estimate = (ewma * decay) + (rtt * recency);
                trace!(
                    "update duration={:03.0}ms decay={:06.0}ns; next={:03.0}ms",
                    rtt / NANOS_PER_MILLI,
                    ewma - next_estimate,
                    next_estimate / NANOS_PER_MILLI,
                );
                *latency = Duration::from_nanos(next_estimate as u64);
                true
            }
        });
    }
}

const NANOS_PER_MILLI: f64 = 1_000_000.0;

// Utility that converts durations to nanos in f64.
//
// Due to a lossy transformation, the maximum value that can be represented is ~585 years,
// which, I hope, is more than enough to represent request latencies.
fn nanos(d: Duration) -> f64 {
    const NANOS_PER_SEC: u64 = 1_000_000_000;
    let n = f64::from(d.subsec_nanos());
    let s = d.as_secs().saturating_mul(NANOS_PER_SEC) as f64;
    n + s
}
