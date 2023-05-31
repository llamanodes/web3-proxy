use serde::ser::Serializer;
use serde::Serialize;
use tokio::time::Duration;

pub struct EwmaLatency {
    /// exponentially weighted of some latency in milliseconds
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
    #[inline]
    pub fn record(&mut self, duration: Duration) {
        self.record_ms(duration.as_secs_f64() * 1000.0);
    }

    #[inline]
    pub fn record_ms(&mut self, milliseconds: f64) {
        // don't let it go under 0.1ms
        self.ewma.add(milliseconds.max(0.1));
    }

    /// Current EWMA value in milliseconds
    #[inline]
    pub fn value(&self) -> f64 {
        self.ewma.value()
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
