use serde::ser::Serializer;
use serde::Serialize;
use tokio::time::Duration;

pub struct EwmaLatency {
    /// exponentially weighted moving average of how many milliseconds behind the fastest node we are
    ewma: ewma::EWMA,
}

impl Serialize for EwmaLatency {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_f64(self.ewma.value())
    }
}

impl EwmaLatency {
    #[inline(always)]
    pub fn record(&mut self, duration: Duration) {
        self.record_ms(duration.as_secs_f64() * 1000.0);
    }

    #[inline(always)]
    pub fn record_ms(&mut self, milliseconds: f64) {
        self.ewma.add(milliseconds);
    }

    /// Current EWMA value in milliseconds
    #[inline(always)]
    pub fn value(&self) -> f64 {
        self.ewma.value()
    }
}

impl Default for EwmaLatency {
    fn default() -> Self {
        // TODO: what should the default span be? 25 requests?
        let span = 25.0;

        let start = 1000.0;

        Self::new(span, start)
    }
}

impl EwmaLatency {
    // depending on the span, start might not be perfect
    pub fn new(span: f64, start_ms: f64) -> Self {
        let alpha = Self::span_to_alpha(span);

        let mut ewma = ewma::EWMA::new(alpha);

        if start_ms > 0.0 {
            for _ in 0..(span as u64) {
                ewma.add(start_ms);
            }
        }

        Self { ewma }
    }

    fn span_to_alpha(span: f64) -> f64 {
        2.0 / (span + 1.0)
    }
}
