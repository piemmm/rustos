//! Deterministic entropy stand-ins the unit tests seed generators from:
//! reproducible from a seed, never entropy, never in a shipping build.

use crate::entropy::{EntropyError, EntropySource};
use tairix_fuzzseed::Prng;

/// A seeded source, optionally failing closed once `budget` fills are spent.
pub(crate) struct SeededSource {
    stream: Prng,
    budget: Option<u32>,
}

impl SeededSource {
    pub(crate) fn new(seed: u64) -> Self {
        Self {
            stream: Prng::new(seed),
            budget: None,
        }
    }

    /// A source that succeeds `n` times, then fails forever — to drive the
    /// reseed-failure and shortage paths.
    pub(crate) fn with_budget(seed: u64, n: u32) -> Self {
        Self {
            stream: Prng::new(seed),
            budget: Some(n),
        }
    }
}

impl EntropySource for SeededSource {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
        if let Some(budget) = self.budget.as_mut() {
            if *budget == 0 {
                return Err(EntropyError::Unavailable);
            }
            *budget -= 1;
        }
        self.stream.fill(out);
        Ok(())
    }
}

/// A source whose non-blocking `fill` is exhausted after `budget` draws but
/// whose `fill_blocking` always delivers — a parking platform source.
pub(crate) struct ParkingSource {
    stream: Prng,
    budget: u32,
}

impl ParkingSource {
    pub(crate) fn new(seed: u64, budget: u32) -> Self {
        Self {
            stream: Prng::new(seed),
            budget,
        }
    }
}

impl EntropySource for ParkingSource {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
        if self.budget == 0 {
            return Err(EntropyError::Unavailable);
        }
        self.budget -= 1;
        self.stream.fill(out);
        Ok(())
    }

    fn fill_blocking(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
        if self.budget == 0 {
            // The wait replenishes the pool, then delivers.
            self.budget = 1;
        }
        self.fill(out)
    }
}
