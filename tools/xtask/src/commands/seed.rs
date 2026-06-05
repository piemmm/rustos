//! Per-run PRNG seed selection for the §19.6 fuzz and §19.7 proptest soaks.
//!
//! The in-tree fuzz harnesses (`commands::fuzz`) and stateful proptest models
//! (`commands::proptest`) draw their inputs from a seeded PRNG. With a *fixed*
//! seed every run — a 60 s `--quick` pass and a 24 h `--soak` alike — replays
//! the identical input stream, so a soak that re-ran the same harness night
//! after night would explore nothing new (`AGENTS.md` §2.1 forbids that kind
//! of busy-work). This module is the single place (`AGENTS.md` §2.2) that
//! chooses the seed each orchestrated run exports to its harnesses:
//!
//! * with no `--seed`, every job gets a *fresh* seed mixed from wall-clock
//!   time, the process id, and a monotonic counter, so consecutive soaks
//!   progress through new inputs;
//! * with `--seed N`, every job gets a *deterministic* seed derived from `N`
//!   and the job index, so a crash a soak reported can be reproduced exactly.
//!
//! The orchestrator always logs the seed it picked, so a non-deterministic
//! soak run is still reproducible: feed the logged value back via `--seed`.
//! This is a *test-input* seed, not a security seed, so it deliberately does
//! not go through `lib/crypto`/`lib/rng` (`AGENTS.md` §22 governs the kernel
//! CSPRNG, not host build tooling).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Environment variable the fuzz orchestrator exports to each harness.
pub const FUZZ_SEED_ENV: &str = "RUSTOS_FUZZ_SEED";

/// Environment variable the proptest orchestrator exports to each model.
pub const PROPTEST_SEED_ENV: &str = "RUSTOS_PROPTEST_SEED";

/// `SplitMix64` finaliser: spreads the bits of `x` so even sequential inputs
/// (a job index, an incrementing counter) yield well-separated seeds.
#[must_use]
pub fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Draw a fresh, hard-to-repeat seed from host entropy.
///
/// Mixes the current wall-clock time, the process id, and a per-process
/// monotonic counter so two calls in the same process (one per registry
/// target) never collide, and two runs of the orchestrator differ.
#[must_use]
pub fn random_seed() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_nanos()).ok())
        .unwrap_or(0);
    let pid = u64::from(std::process::id());
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);

    splitmix64(nanos) ^ splitmix64(pid).rotate_left(17) ^ splitmix64(count).rotate_left(43)
}

/// The seed for the `index`-th job of a run.
///
/// `None` requests a fresh entropy seed; `Some(base)` derives a deterministic
/// per-job seed from `base` so an explicit `--seed` reproduces a whole run
/// while still giving each harness a distinct stream.
#[must_use]
pub fn job_seed(base: Option<u64>, index: usize) -> u64 {
    match base {
        Some(base) => splitmix64(base ^ splitmix64(u64::try_from(index).unwrap_or(u64::MAX))),
        None => random_seed(),
    }
}

#[cfg(test)]
mod tests {
    use super::{job_seed, random_seed, splitmix64};

    #[test]
    fn splitmix64_separates_sequential_inputs() {
        // Adjacent counter values must not produce adjacent seeds.
        let a = splitmix64(0);
        let b = splitmix64(1);
        assert_ne!(a, b);
        assert!(
            a.abs_diff(b) > 1,
            "sequential inputs collapsed to neighbours"
        );
    }

    #[test]
    fn random_seed_differs_across_calls() {
        // The monotonic counter alone guarantees distinct seeds within a
        // single process even if the clock does not advance between calls.
        let seeds: Vec<u64> = (0..16).map(|_| random_seed()).collect();
        for (i, a) in seeds.iter().enumerate() {
            for b in &seeds[i + 1..] {
                assert_ne!(a, b, "two entropy draws collided");
            }
        }
    }

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
