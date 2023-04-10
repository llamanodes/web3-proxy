//! A counter of events in a time period.
use std::collections::VecDeque;
use tokio::time::{Duration, Instant};

/// Measures ticks in a time period.
#[derive(Debug)]
pub struct RateCounter {
    period: Duration,
    items: VecDeque<Instant>,
}

impl RateCounter {
    pub fn new(period: Duration) -> Self {
        let items = VecDeque::new();

        Self { period, items }
    }

    /// update the counter and return the rate for the current period
    /// true if the current time should be counted
    pub fn update(&mut self, tick: bool) -> usize {
        let now = Instant::now();
        let too_old = now - self.period;

        while self.items.front().map_or(false, |t| *t < too_old) {
            self.items.pop_front();
        }

        if tick {
            self.items.push_back(now);
        }

        self.items.len()
    }
}
