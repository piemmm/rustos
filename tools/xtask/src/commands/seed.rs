//! Per-run PRNG seed selection for the fuzz and proptest soaks.
//!
//! The in-tree fuzz harnesses (`commands::fuzz`) and stateful proptest models
//! (`commands::proptest`) draw their inputs from a seeded PRNG. With a *fixed*
//! seed every run replays the identical input stream, so a soak that re-ran
//! the same harness night after night would explore nothing new (the charter forbids that kind of busy-work). This module chooses the seed each
//! orchestrated run exports to its harnesses:
//!
//! * with no `--seed`, every job gets a *fresh* entropy seed, so consecutive
//!   soaks progress through new inputs;
//! * with `--seed N`, every job gets a *deterministic* seed derived from `N`
//!   and the job index, so a crash a soak reported can be reproduced exactly.
//!
//! The orchestrator always logs the seed it picked (in the per-job label), so
//! a non-deterministic soak run is still reproducible: feed the logged value
//! back via `--seed`.
//!
//! The seed primitives and the env-var names are shared with the harness side
//! through [`tairix_fuzzseed`] so they live in exactly one place: this module adds only the orchestrator-specific per-job derivation.

pub use tairix_fuzzseed::{splitmix64, FUZZ_SEED_ENV, PROPTEST_SEED_ENV};

/// The seed for the `index`-th job of a run.
///
/// `None` requests a fresh entropy seed; `Some(base)` derives a deterministic
/// per-job seed from `base` so an explicit `--seed` reproduces a whole run
/// while still giving each harness a distinct stream.
#[must_use]
pub fn job_seed(base: Option<u64>, index: usize) -> u64 {
    match base {
        Some(base) => splitmix64(base ^ splitmix64(u64::try_from(index).unwrap_or(u64::MAX))),
        None => tairix_fuzzseed::entropy_seed(),
    }
}

#[cfg(test)]
mod tests {
    use super::job_seed;

    #[test]
    fn explicit_base_is_reproducible_and_distinct_per_index() {
        // Same (base, index) reproduces; different indices diverge.
        assert_eq!(job_seed(Some(42), 0), job_seed(Some(42), 0));
        assert_eq!(job_seed(Some(42), 3), job_seed(Some(42), 3));
        assert_ne!(job_seed(Some(42), 0), job_seed(Some(42), 1));
        assert_ne!(job_seed(Some(42), 0), job_seed(Some(43), 0));
    }

    #[test]
    fn no_base_draws_fresh_seeds() {
        assert_ne!(job_seed(None, 0), job_seed(None, 0));
    }
}
