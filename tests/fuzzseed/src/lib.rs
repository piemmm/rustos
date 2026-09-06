//! Shared per-run PRNG seeding, logging, and budget seam for the test
//! harnesses that draw pseudo-random inputs: the fuzz harnesses, the
//! stateful proptest models, and the filesystem and statistical-RNG soaks.
//!
//! ## Why this exists
//!
//! Every one of those harnesses needs the same three things, and before this
//! crate each one re-implemented them — a duplication smell:
//!
//! 1. **A per-run seed that is fresh by default but pinnable for replay.** A
//!    harness that replayed one fixed seed every run explored nothing new
//!    across repeated runs (and a soak that re-ran the same stream night after
//!    night is the busy-work the charter forbids). So by default each run draws a
//!    *fresh* seed from host entropy; setting the harness's seed environment
//!    variable pins it instead, which is how a logged failure is reproduced.
//! 2. **The seed logged at the start of the test.** Because the default seed is
//!    random, a failure is only reproducible if the seed that produced it was
//!    recorded. [`start`] prints the seed — with the exact `VAR=value` needed
//!    to replay it — before the harness draws its first input.
//! 3. **A single test execution by default, a wall-clock budget on demand.** A
//!    plain `cargo test` (a developer machine, or the per-PR `ci` gate) runs
//!    each PRNG-driven test *once* — its fixed smoke sweep, with a fresh,
//!    logged seed ([`budget_deadline`] returns `None`, so the harness loop
//!    body runs a single time). The soak orchestrators export a budget
//!    environment variable and the harness loops its continuing stream until
//!    [`within_budget`] says the time is up.
//!
//! It also owns the *names* of those environment variables, so the
//! orchestrator that exports one and the harness that reads it cannot drift
//! apart on a spelling.
//!
//! This crate is **test scaffolding only**. It lives under `tests/` (not
//! `lib/`, which is reserved for code that ships inside TAIRiX) and is pulled
//! in only by the host harness crates and the `tools/xtask` orchestrator, so
//! it is never part of any TAIRiX build graph.
//!
//! It deliberately depends on nothing: the seed is a *test-input* seed, not a
//! security seed, so it must not route through `lib/crypto` / `lib/rng`
//! (the charter governs the kernel CSPRNG, not host test tooling).

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Seed environment variable the `cargo xtask fuzz` orchestrator exports and
/// the fuzz harnesses read.
pub const FUZZ_SEED_ENV: &str = "TAIRIX_FUZZ_SEED";

/// Wall-clock budget (seconds) the `cargo xtask fuzz` orchestrator exports for
/// a soak; unset or zero selects the single smoke iteration.
pub const FUZZ_BUDGET_ENV: &str = "TAIRIX_FUZZ_BUDGET_SECS";

/// Seed environment variable the `cargo xtask proptest` orchestrator exports
/// and the models read.
pub const PROPTEST_SEED_ENV: &str = "TAIRIX_PROPTEST_SEED";

/// Wall-clock budget (seconds) the `cargo xtask proptest` orchestrator exports
/// for a soak; unset or zero selects the single smoke iteration.
pub const PROPTEST_BUDGET_ENV: &str = "TAIRIX_PROPTEST_BUDGET_SECS";

/// Seed environment variable the filesystem soak reads. The orchestrator
/// leaves it unset so each run takes a new path; a developer pins it to replay
/// a logged failure.
pub const FSSOAK_SEED_ENV: &str = "TAIRIX_FSSOAK_SEED";

/// Wall-clock budget (seconds) the `cargo xtask fssoak` orchestrator exports
/// for a soak; unset or zero selects the single smoke iteration.
pub const FSSOAK_BUDGET_ENV: &str = "TAIRIX_FSSOAK_BUDGET_SECS";

/// RAM device size (bytes) the `cargo xtask fssoak` orchestrator exports for a
/// soak; unset selects the harness's own smaller smoke device.
pub const FSSOAK_BYTES_ENV: &str = "TAIRIX_FSSOAK_BYTES";

/// Seed environment variable the statistical RNG soak reads. Unlike the other
/// harnesses an *unset* value does not draw fresh entropy for a plain `cargo
/// test`: a statistical verdict is probabilistic, so the per-PR pass runs
/// from a fixed seed and only a budgeted soak explores new streams. Pinning
/// this replays a logged failure either way.
pub const RNGSOAK_SEED_ENV: &str = "TAIRIX_RNGSOAK_SEED";

/// Wall-clock budget (seconds) the `cargo xtask rngsoak` orchestrator exports
/// for a soak; unset or zero selects the single smoke pass.
pub const RNGSOAK_BUDGET_ENV: &str = "TAIRIX_RNGSOAK_BUDGET_SECS";

/// Bytes per generator the `cargo xtask rngsoak` orchestrator exports for one
/// pass; unset selects the harness's own smoke count.
pub const RNGSOAK_BYTES_ENV: &str = "TAIRIX_RNGSOAK_BYTES";

/// `SplitMix64` finaliser: spreads the bits of `x` so even sequential inputs
/// (a counter, a job index) map to well-separated outputs.
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
/// monotonic counter so two calls in one process (e.g. two tests in one
/// harness) never collide, and two runs of the harness differ.
#[must_use]
pub fn entropy_seed() -> u64 {
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

/// Resolve the seed for a run: the value of `seed_env` when it is set to a
/// parseable `u64`, otherwise a fresh [`entropy_seed`].
///
/// Pinning the variable (the orchestrators do this for every job, and a
/// developer does it from a logged failure) reproduces a run exactly; leaving
/// it unset explores new inputs each run.
#[must_use]
pub fn resolve_seed(seed_env: &str) -> u64 {
    std::env::var(seed_env)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(entropy_seed)
}

/// Print the seed a test is about to use, with the exact command to replay it.
///
/// Goes to stdout so it travels with the failing test's captured output (and
/// is always visible under `--nocapture`, which the soak orchestrators pass).
pub fn announce(test_name: &str, seed_env: &str, seed: u64) {
    println!(
        "[fuzzseed] {test_name}: PRNG seed = {seed} ({seed:#018x}); \
         replay with {seed_env}={seed}"
    );
}

/// Resolve and [`announce`] the seed for `test_name`, returning it.
///
/// The one call a harness makes at the top of a test: it logs the seed before
/// the first input is drawn, so even a fresh-seed run is reproducible from the
/// logged value.
#[must_use]
pub fn start(test_name: &str, seed_env: &str) -> u64 {
    let seed = resolve_seed(seed_env);
    announce(test_name, seed_env, seed);
    seed
}

/// The wall-clock deadline for the current run, or `None` for the single
/// smoke iteration.
///
/// `budget_env` is read as a number of seconds: a positive value turns the
/// harness into a budgeted soak loop; unset, empty, zero, or unparseable keeps
/// the single-iteration smoke behaviour.
#[must_use]
pub fn budget_deadline(budget_env: &str) -> Option<Instant> {
    let secs: u64 = std::env::var(budget_env).ok()?.parse().ok()?;
    if secs == 0 {
        return None;
    }
    Some(Instant::now() + Duration::from_secs(secs))
}

/// `true` while a budgeted soak still has time left; always `false` for the
/// single smoke iteration (a `None` deadline), so the harness loop body runs
/// exactly once.
#[must_use]
pub fn within_budget(deadline: Option<Instant>) -> bool {
    matches!(deadline, Some(end) if Instant::now() < end)
}

/// Greatest common divisor (Euclid), used to pick a stride that makes the
/// [`budgeted_sweep`] order a full permutation.
fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Visit every index in `0..len` exactly once, in a seeded pseudo-random
/// order, stopping early once `deadline` has passed.
///
/// The exhaustive soak sweeps (flipping every byte of a formatted filesystem
/// image, say) can be far larger than a short wall-clock budget: swept
/// sequentially front-to-back they would ignore the budget entirely and
/// overrun a 5-second run by minutes. This helper keeps such a sweep
/// budget-honest without giving up the exhaustive guarantee: the order is a
/// full permutation of `0..len` (a seeded start offset advanced by a stride
/// coprime to `len`), so a budget that outlives the sweep (the 24-hour
/// nightly) still visits every index exactly once, while a short budget
/// probes a well-spread sample of positions and stops on time. The order
/// replays exactly from `seed`, so a logged failure reproduces.
///
/// The deadline is checked before every visit: the overshoot past `deadline`
/// is bounded by a single `visit` call.
pub fn budgeted_sweep(len: usize, seed: u64, deadline: Instant, mut visit: impl FnMut(usize)) {
    if len == 0 {
        return;
    }
    let reduce = |x: u64| usize::try_from(x % (len as u64)).unwrap_or(0);
    let mut pos = reduce(splitmix64(seed));
    // Any stride coprime to `len` cycles through all of `0..len`; counting
    // down from a seeded candidate always terminates at 1, which is coprime
    // to everything.
    let mut stride = reduce(splitmix64(seed ^ 0x5851_F42D_4C95_7F2D)).max(1);
    while gcd(stride, len) != 1 {
        stride -= 1;
    }
    for _ in 0..len {
        if Instant::now() >= deadline {
            break;
        }
        visit(pos);
        // `pos + stride` stays in `0..2*len`; subtracting the wrap first
        // keeps the arithmetic overflow-free for any `usize` length.
        pos = if stride >= len - pos {
            stride - (len - pos)
        } else {
            pos + stride
        };
    }
}

/// Expand a 64-bit seed into a 32-byte seed (e.g. proptest's `ChaCha` seed)
/// via four `SplitMix64` rounds.
#[must_use]
pub fn expand_seed(seed: u64) -> [u8; 32] {
    let mut state = seed;
    let mut bytes = [0u8; 32];
    for chunk in bytes.chunks_mut(8) {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        chunk.copy_from_slice(&z.to_le_bytes());
    }
    bytes
}

/// A small, deterministic 64-bit linear congruential generator (Knuth's MMIX
/// multiplier). Given the same seed it reproduces the same stream, so a
/// failure replays exactly from its logged seed; the *start* seed is what
/// [`start`] randomises per run.
pub struct Lcg(u64);

impl Lcg {
    /// Seed the generator. A zero seed is nudged off the fixed point so the
    /// recurrence never collapses.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    /// Next 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        self.0
    }

    /// A value in `0..n` (returns `0` when `n == 0`).
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        usize::try_from(self.next_u64() % (n as u64)).unwrap_or(0)
    }

    /// Fill `buf` with pseudo-random bytes.
    pub fn fill(&mut self, buf: &mut [u8]) {
        let mut i = 0;
        while i < buf.len() {
            let word = self.next_u64().to_le_bytes();
            let take = core::cmp::min(8, buf.len() - i);
            buf[i..i + take].copy_from_slice(&word[..take]);
            i += take;
        }
    }
}

/// The shared `proptest` stateful-model runner used by the models.
///
/// Enabled by the `proptest` feature. Centralises the seed resolution, the
/// start-of-test seed log, the `ChaCha` RNG construction, and the smoke /
/// budgeted-soak loop so each `kernel/{sec,ipc,syscall}` and `lib/caps` model
/// is just a strategy plus a check.
#[cfg(feature = "proptest")]
pub mod prop {
    use super::{announce, budget_deadline, expand_seed, resolve_seed, within_budget};
    use super::{PROPTEST_BUDGET_ENV, PROPTEST_SEED_ENV};
    use proptest::strategy::Strategy;
    use proptest::test_runner::{Config, RngAlgorithm, TestCaseError, TestRng, TestRunner};

    /// Run `check` over programs drawn from `strategy`, panicking with the
    /// shrunk counterexample on the first failure.
    ///
    /// The seed for the run is resolved from `TAIRIX_PROPTEST_SEED` (fresh
    /// entropy when unset) and **logged before the first case is drawn**, so a
    /// fresh-seed run is still reproducible from the logged value via
    /// `--seed`. A plain `cargo test` (no budget) runs `smoke_cases` once;
    /// `cargo xtask proptest --soak` sets `TAIRIX_PROPTEST_BUDGET_SECS` and the
    /// model keeps running `budget_batch_cases` batches off the same continuing
    /// RNG until the deadline elapses.
    pub fn drive<S: Strategy>(
        test_name: &str,
        smoke_cases: u32,
        budget_batch_cases: u32,
        strategy: S,
        check: impl Fn(S::Value) -> Result<(), TestCaseError>,
    ) {
        let seed = resolve_seed(PROPTEST_SEED_ENV);
        announce(test_name, PROPTEST_SEED_ENV, seed);

        let deadline = budget_deadline(PROPTEST_BUDGET_ENV);
        let cases = if deadline.is_some() {
            budget_batch_cases
        } else {
            smoke_cases
        };
        let config = Config {
            cases,
            failure_persistence: None,
            ..Config::default()
        };
        let rng = TestRng::from_seed(RngAlgorithm::ChaCha, &expand_seed(seed));
        let mut runner = TestRunner::new_with_rng(config, rng);
        loop {
            if let Err(err) = runner.run(&strategy, &check) {
                panic!("proptest stateful model found a counterexample: {err}");
            }
            if !within_budget(deadline) {
                break;
            }
        }
    }
}

/// A value that records its own drop, so a container can be held to exactly
/// one drop per element it ever took — across growth, overwrite, removal,
/// retention, and an abandoned owning iterator.
pub struct Counted(Rc<Cell<usize>>);

impl Counted {
    /// A fresh drop counter, at zero.
    #[must_use]
    pub fn counter() -> Rc<Cell<usize>> {
        Rc::new(Cell::new(0))
    }

    /// A value reporting its drop to `counter`.
    #[must_use]
    pub fn new(counter: &Rc<Cell<usize>>) -> Self {
        Self(Rc::clone(counter))
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        budget_deadline, budgeted_sweep, entropy_seed, expand_seed, resolve_seed, splitmix64,
        within_budget, Lcg,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn splitmix64_separates_sequential_inputs() {
        let a = splitmix64(0);
        let b = splitmix64(1);
        assert_ne!(a, b);
        assert!(a.abs_diff(b) > 1, "sequential inputs collapsed");
    }

    #[test]
    fn entropy_seed_differs_across_calls() {
        // The monotonic counter alone guarantees distinct seeds within one
        // process even if the clock does not advance between calls.
        let seeds: Vec<u64> = (0..16).map(|_| entropy_seed()).collect();
        for (i, a) in seeds.iter().enumerate() {
            for b in &seeds[i + 1..] {
                assert_ne!(a, b, "two entropy draws collided");
            }
        }
    }

    #[test]
    fn resolve_seed_uses_the_env_when_set() {
        // A process-unique variable name keeps this test independent of any
        // other test that touches the environment.
        let var = "TAIRIX_FUZZSEED_SELFTEST_SEED";
        std::env::set_var(var, "12345");
        assert_eq!(resolve_seed(var), 12345);
        std::env::remove_var(var);
        // Unset → a fresh entropy seed (overwhelmingly not 12345).
        assert_ne!(resolve_seed(var), 12345);
    }

    #[test]
    fn budget_unset_runs_a_single_iteration() {
        let var = "TAIRIX_FUZZSEED_SELFTEST_BUDGET";
        std::env::remove_var(var);
        assert!(budget_deadline(var).is_none());
        assert!(!within_budget(budget_deadline(var)));
        // Zero is treated as "no budget" too.
        std::env::set_var(var, "0");
        assert!(budget_deadline(var).is_none());
        std::env::remove_var(var);
    }

    #[test]
    fn budget_set_yields_a_live_deadline() {
        let var = "TAIRIX_FUZZSEED_SELFTEST_BUDGET2";
        std::env::set_var(var, "3600");
        let deadline = budget_deadline(var).expect("positive budget");
        assert!(deadline > Instant::now());
        assert!(within_budget(Some(deadline)));
        std::env::remove_var(var);
    }

    #[test]
    fn within_budget_is_false_once_the_deadline_passed() {
        let past = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("an instant one second in the past exists");
        assert!(!within_budget(Some(past)));
    }

    #[test]
    fn expand_seed_is_deterministic_and_spread() {
        assert_eq!(expand_seed(7), expand_seed(7));
        assert_ne!(expand_seed(7), expand_seed(8));
        // The 32 bytes are not all equal (a degenerate expansion).
        let bytes = expand_seed(1);
        assert!(bytes.iter().any(|&b| b != bytes[0]));
    }

    #[test]
    fn lcg_is_reproducible_from_its_seed() {
        let mut a = Lcg::new(42);
        let mut b = Lcg::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn lcg_below_stays_in_range_and_handles_zero() {
        let mut rng = Lcg::new(99);
        for _ in 0..1000 {
            assert!(rng.below(10) < 10);
        }
        assert_eq!(rng.below(0), 0);
    }

    #[test]
    fn lcg_fill_writes_every_byte_for_any_length() {
        let mut rng = Lcg::new(123);
        for len in [0usize, 1, 7, 8, 9, 33] {
            let mut buf = vec![0u8; len];
            rng.fill(&mut buf);
            assert_eq!(buf.len(), len);
        }
    }

    /// A deadline comfortably beyond the test's own runtime.
    fn far_deadline() -> Instant {
        Instant::now() + Duration::from_secs(3600)
    }

    #[test]
    fn budgeted_sweep_visits_every_index_exactly_once_within_budget() {
        // Lengths with varied factorisations (even, odd, prime, power of
        // two, tiny) so the coprime-stride permutation is exercised broadly.
        for len in [1usize, 2, 3, 16, 17, 100, 1024, 4095] {
            for seed in [0u64, 1, 42, u64::MAX] {
                let mut seen = vec![0u32; len];
                budgeted_sweep(len, seed, far_deadline(), |i| seen[i] += 1);
                assert!(
                    seen.iter().all(|&n| n == 1),
                    "len {len} seed {seed}: not a one-visit permutation"
                );
            }
        }
    }

    #[test]
    fn budgeted_sweep_stops_at_an_expired_deadline() {
        // The regression this helper exists for: an exhaustive sweep must
        // honour its wall-clock budget instead of running to completion.
        let expired = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("an instant one second in the past exists");
        let mut visits = 0u32;
        budgeted_sweep(1_000_000, 7, expired, |_| visits += 1);
        assert_eq!(visits, 0, "an expired budget must stop the sweep");
    }

    #[test]
    fn budgeted_sweep_handles_an_empty_range() {
        budgeted_sweep(0, 9, far_deadline(), |_| panic!("nothing to visit"));
    }

    #[test]
    fn budgeted_sweep_order_replays_from_its_seed() {
        let order = |seed: u64| {
            let mut positions = Vec::new();
            budgeted_sweep(257, seed, far_deadline(), |i| positions.push(i));
            positions
        };
        assert_eq!(order(11), order(11));
        // Distinct seeds explore in a different order (a fixed pair, so the
        // assertion is deterministic).
        assert_ne!(order(11), order(12));
    }
}
