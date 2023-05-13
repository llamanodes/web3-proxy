use std::sync::atomic::{AtomicU64, Ordering};

/// Implements an atomic pair of f32s
///
/// This uses an AtomicU64 internally.
#[derive(Debug)]
pub struct AtomicF32Pair(AtomicU64);

impl AtomicF32Pair {
    /// Creates a new atomic pair.
    pub fn new(pair: [f32; 2]) -> Self {
        Self(AtomicU64::new(to_bits(pair)))
    }

    /// Loads a value from the atomic pair.
    pub fn load(&self, ordering: Ordering) -> [f32; 2] {
        from_bits(self.0.load(ordering))
    }

    /// Fetches the value, and applies a function to it that returns an
    /// optional new value. Returns a Result of Ok(previous_value) if
    /// the function returned Some(_), else Err(previous_value).
    pub fn fetch_update<F>(
        &self,
        set_order: Ordering,
        fetch_order: Ordering,
        mut f: F,
    ) -> Result<[f32; 2], [f32; 2]>
    where
        F: FnMut([f32; 2]) -> Option<[f32; 2]>,
    {
        self.0
            .fetch_update(set_order, fetch_order, |bits| {
                f(from_bits(bits)).map(to_bits)
            })
            .map(from_bits)
            .map_err(from_bits)
    }
}

/// Convert a f32 pair to its bit-representation as u64
fn to_bits(pair: [f32; 2]) -> u64 {
    let f1 = pair[0].to_bits() as u64;
    let f2 = pair[1].to_bits() as u64;
    (f1 << 32) | f2
}

/// Build a f32 pair from its bit-representation as u64
fn from_bits(bits: u64) -> [f32; 2] {
    let f1 = f32::from_bits((bits >> 32) as u32);
    let f2 = f32::from_bits(bits as u32);
    [f1, f2]
}

#[cfg(test)]
mod tests {
    use std::f32;
    use std::sync::atomic::Ordering;

    use super::{from_bits, to_bits, AtomicF32Pair};

    #[test]
    fn test_f32_pair_bit_conversions() {
        let pair = [f32::consts::PI, f32::consts::E];
        assert_eq!(pair, from_bits(to_bits(pair)));
    }

    #[test]
    fn test_atomic_f32_pair_load() {
        let pair = [f32::consts::PI, f32::consts::E];
        let atomic = AtomicF32Pair::new(pair);
        assert_eq!(pair, atomic.load(Ordering::Relaxed));
    }

    #[test]
    fn test_atomic_f32_pair_fetch_update() {
        let pair = [f32::consts::PI, f32::consts::E];
        let atomic = AtomicF32Pair::new(pair);
        atomic
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |[f1, f2]| {
                Some([f1 + 1.0, f2 + 1.0])
            })
            .unwrap();
        assert_eq!(
            [pair[0] + 1.0, pair[1] + 1.0],
            atomic.load(Ordering::Relaxed)
        );
    }
}
