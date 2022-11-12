//! works just like rand::thread_rng but with a rng that is **not** cryptographically secure
//!
//! TODO: currently uses Xoshiro256Plus. do some benchmarks
pub use rand;

use rand::{Error, Rng, RngCore, SeedableRng};
use rand_xoshiro::Xoshiro256Plus;
use std::{cell::UnsafeCell, rc::Rc};

#[derive(Clone, Debug)]
pub struct ThreadFastRng {
    // Rc is explicitly !Send and !Sync
    rng: Rc<UnsafeCell<Xoshiro256Plus>>,
}

thread_local! {
    pub static THREAD_FAST_RNG: Rc<UnsafeCell<Xoshiro256Plus>> = {
        // use a cryptographically secure rng for the seed
        let mut crypto_rng = rand::thread_rng();
        let seed = crypto_rng.gen();
        // use a fast rng for things that aren't cryptography
        let rng = Xoshiro256Plus::seed_from_u64(seed);
        Rc::new(UnsafeCell::new(rng))
    };
}

pub fn thread_fast_rng() -> ThreadFastRng {
    let rng = THREAD_FAST_RNG.with(|t| t.clone());
    ThreadFastRng { rng }
}

impl Default for ThreadFastRng {
    fn default() -> Self {
        thread_fast_rng()
    }
}

impl RngCore for ThreadFastRng {
    #[inline(always)]
    fn next_u32(&mut self) -> u32 {
        // SAFETY: We must make sure to stop using `rng` before anyone else
        // creates another mutable reference
        let rng = unsafe { &mut *self.rng.get() };
        rng.next_u32()
    }

    #[inline(always)]
    fn next_u64(&mut self) -> u64 {
        // SAFETY: We must make sure to stop using `rng` before anyone else
        // creates another mutable reference
        let rng = unsafe { &mut *self.rng.get() };
        rng.next_u64()
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        // SAFETY: We must make sure to stop using `rng` before anyone else
        // creates another mutable reference
        let rng = unsafe { &mut *self.rng.get() };
        rng.fill_bytes(dest)
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Error> {
        // SAFETY: We must make sure to stop using `rng` before anyone else
        // creates another mutable reference
        let rng = unsafe { &mut *self.rng.get() };
        rng.try_fill_bytes(dest)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_thread_fast_rng() {
        let mut r = thread_fast_rng();
        r.gen::<i32>();
        assert_eq!(r.gen_range(0..1), 0);
    }

    #[test]
    fn test_thread_fast_rng_struct() {
        let mut r = ThreadFastRng::default();
        r.gen::<i32>();
        assert_eq!(r.gen_range(0..1), 0);
    }
}
