use tokio::time::Duration;

pub const NANOS_PER_MILLI: f64 = 1_000_000.0;

/// Utility that converts durations to nanos in f64.
///
/// Due to a lossy transformation, the maximum value that can be represented is ~585 years,
/// which, I hope, is more than enough to represent request latencies.
pub fn nanos(d: Duration) -> f64 {
    const NANOS_PER_SEC: u64 = 1_000_000_000;
    let n = f64::from(d.subsec_nanos());
    let s = d.as_secs().saturating_mul(NANOS_PER_SEC) as f64;
    n + s
}
