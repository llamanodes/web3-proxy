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

#[cfg(test)]
mod tests {
    use tokio::time::Duration;

    #[test]
    fn nanos() {
        assert_eq!(super::nanos(Duration::new(0, 0)), 0.0);
        assert_eq!(super::nanos(Duration::new(0, 123)), 123.0);
        assert_eq!(super::nanos(Duration::new(1, 23)), 1_000_000_023.0);
        assert_eq!(
            super::nanos(Duration::new(::std::u64::MAX, 999_999_999)),
            18446744074709553000.0
        );
    }
}
