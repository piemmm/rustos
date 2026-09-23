//! Shared host-test entropy stand-in: a seeded source, reproducible and never
//! entropy, so a reserve seeded from it draws deterministically.

use tairix_fuzzseed::Prng;
use tairix_rng::{EntropyError, EntropySource};

/// An entropy source filling from the stream `seed` fixes.
pub(crate) struct SeededEntropy(Prng);

impl SeededEntropy {
    pub(crate) fn new(seed: u64) -> Self {
        Self(Prng::new(seed))
    }
}

impl EntropySource for SeededEntropy {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
        self.0.fill(out);
        Ok(())
    }
}
