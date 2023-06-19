use crate::util::span::span_to_alpha;
use serde::ser::Serializer;
use serde::Serialize;
use tokio::time::Duration;
use watermill::ewmean::EWMean;
use watermill::stats::Univariate;

pub struct EwmaLatency {
    /// exponentially weighted of some latency in milliseconds
    /// TODO: compare crates: ewma vs watermill
    seconds: EWMean<f32>,
}

/// serialize as milliseconds
impl Serialize for EwmaLatency {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_f32(self.seconds.get() * 1000.0)
    }
}

impl EwmaLatency {
    #[inline]
    pub fn record(&mut self, duration: Duration) {
        self.record_secs(duration.as_secs_f32());
    }

    #[inline]
    pub fn record_secs(&mut self, secs: f32) {
        // TODO: we could change this to use a channel like the peak_ewma and rolling_quantile code, but this is fine if it updates on insert instead of async
        self.seconds.update(secs);
    }

    /// Current EWMA value in seconds
    #[inline]
    pub fn value(&self) -> f32 {
        self.seconds.get()
    }

    /// Current EWMA value in seconds
    #[inline]
    pub fn latency(&self) -> Duration {
        let x = self.seconds.get();

        Duration::from_secs_f32(x)
    }
}

impl Default for EwmaLatency {
    fn default() -> Self {
        // TODO: what should the default span be? 10 requests?
        let span = 10.0;

        // TODO: what should the defautt start be?
        let start = 1.0;

        Self::new(span, start)
    }
}

impl EwmaLatency {
    // depending on the span, start might not be perfect
    pub fn new(span: f32, start_ms: f32) -> Self {
        let alpha = span_to_alpha(span);

        let mut seconds = EWMean::new(alpha);

        if start_ms > 0.0 {
            for _ in 0..(span as u64) {
                seconds.update(start_ms);
            }
        }

        Self { seconds }
    }
}
