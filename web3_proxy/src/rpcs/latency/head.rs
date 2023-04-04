use serde::ser::Serializer;
use serde::Serialize;
use tokio::time::Duration;

pub struct HeadLatency {
    /// exponentially weighted moving average of how many milliseconds behind the fastest node we are
    ewma: ewma::EWMA,
}

impl Serialize for HeadLatency {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_f64(self.ewma.value())
    }
}

impl HeadLatency {
    #[inline(always)]
    pub fn record(&mut self, duration: Duration) {
        self.record_ms(duration.as_secs_f64() * 1000.0);
    }

    #[inline(always)]
    pub fn record_ms(&mut self, milliseconds: f64) {
        self.ewma.add(milliseconds);
    }

    #[inline(always)]
    pub fn value(&self) -> f64 {
        self.ewma.value()
    }
}

impl Default for HeadLatency {
    fn default() -> Self {
        // TODO: what should the default span be? 25 requests? have a "new"
        let span = 25.0;

        let start = 1000.0;

        Self::new(span, start)
    }
}

impl HeadLatency {
    // depending on the span, start might not be perfect
    pub fn new(span: f64, start: f64) -> Self {
        let alpha = Self::span_to_alpha(span);

        let mut ewma = ewma::EWMA::new(alpha);

        if start > 0.0 {
            for _ in 0..(span as u64) {
                ewma.add(start);
            }
        }

        Self { ewma }
    }

    fn span_to_alpha(span: f64) -> f64 {
        2.0 / (span + 1.0)
    }
}
