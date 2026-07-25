//! The bounded, deterministic boot microbenchmark.
//!
//! When a [`Family`](crate::Family) selects
//! [`ByBenchmark`](crate::Selection::ByBenchmark), the
//! [`Selector`](crate::Selector) hands the verified survivors to a
//! [`BenchHarness`] to find the fastest. The harness measures *cycles*, not
//! wall-clock time, through an injected [`CycleCounter`] — so the crate stays
//! `no_std` and names no architecture: `kernel/core` adapts the Arch HAL
//! `CpuCycles` counter to this trait, and host tests drive a deterministic
//! fake.
//!
//! The measurement is **bounded and one-shot**: a fixed number of `iters`
//! calls per round over a fixed, warmed input, and a fixed number of `rounds`
//! reduced by the **median** to reject a scheduling or interrupt outlier. It
//! never loops until a threshold and never busy-waits — the charter forbids
//! both. The only nondeterminism the framework introduces is which
//! verified-correct candidate wins; an operator pin makes even that
//! deterministic.

/// A per-core cycle counter the benchmark measures over.
///
/// This is the generic seam the harness consumes; the architecture-specific
/// counter (x86_64 `RDTSC`, aarch64 `CNTVCT_EL0`, riscv64 `time`) lives behind
/// the Arch HAL `CpuCycles` slice, and `kernel/core` adapts it to this trait.
/// Keeping the seam here means the framework never names an architecture
/// (§2.20) and is fully host-testable with a fake counter.
pub trait CycleCounter {
    /// The current value of the core's cycle counter.
    ///
    /// Successive reads on one core are non-decreasing within a measurement
    /// window (the harness measures a difference, so a counter that wraps at
    /// the 64-bit boundary is handled by [`u64::wrapping_sub`]).
    fn cycles(&self) -> u64;

    /// `true` if the counter advances at a fixed rate regardless of the core's
    /// current frequency (an invariant-TSC-class counter). Advisory only: the
    /// harness compares candidates measured back-to-back on the *same* core in
    /// the *same* window, so a non-invariant counter still yields a valid
    /// relative ranking.
    fn cycles_monotonic_hint(&self) -> bool;
}

/// A bounded, deterministic one-shot microbenchmark.
///
/// Constructed with the [`CycleCounter`] to measure over and the fixed
/// `iters`/`rounds` budget. `iters` is the number of back-to-back calls timed
/// in one round (amortising the counter-read overhead); `rounds` is the number
/// of independent measurements reduced by the median.
pub struct BenchHarness<'c> {
    cycles: &'c dyn CycleCounter,
    iters: u32,
    rounds: u32,
}

impl<'c> BenchHarness<'c> {
    /// The default number of timed calls per round.
    pub const DEFAULT_ITERS: u32 = 256;
    /// The default number of independent rounds reduced by the median.
    pub const DEFAULT_ROUNDS: u32 = 9;

    /// Construct a harness with the default budget
    /// ([`DEFAULT_ITERS`](Self::DEFAULT_ITERS) /
    /// [`DEFAULT_ROUNDS`](Self::DEFAULT_ROUNDS)).
    #[must_use]
    pub fn new(cycles: &'c dyn CycleCounter) -> Self {
        Self {
            cycles,
            iters: Self::DEFAULT_ITERS,
            rounds: Self::DEFAULT_ROUNDS,
        }
    }

    /// Construct a harness with an explicit budget.
    ///
    /// `iters` is clamped to at least 1 and `rounds` to `1..=MAX_ROUNDS` so the
    /// measurement is always well-defined (a zero budget would measure nothing)
    /// and every round's sample lives on a fixed stack buffer; the budget stays
    /// bounded and one-shot regardless of the values passed.
    #[must_use]
    pub fn with_budget(cycles: &'c dyn CycleCounter, iters: u32, rounds: u32) -> Self {
        Self {
            cycles,
            iters: iters.max(1),
            rounds: rounds.clamp(1, MAX_ROUNDS),
        }
    }

    /// The number of timed calls per round.
    #[must_use]
    pub fn iters(&self) -> u32 {
        self.iters
    }

    /// The number of independent measurement rounds.
    #[must_use]
    pub fn rounds(&self) -> u32 {
        self.rounds
    }

    /// Return the index (into `impls`) of the survivor with the lowest median
    /// cycle count over `warm`.
    ///
    /// `impls` is the set of verified-survivor implementation handles in
    /// declared-priority order. Ties break to the **earliest** index (lowest
    /// declared priority) so the result is deterministic when two candidates
    /// measure identically. The selector only benchmarks a non-empty survivor
    /// set; an empty slice yields `0`, the safe default.
    ///
    /// `run` invokes a survivor's implementation on the input, and the harness
    /// discards the result through [`core::hint::black_box`] so the optimiser
    /// cannot elide the work it is measuring.
    #[must_use]
    pub fn fastest<T, In, Out>(&self, impls: &[T], run: fn(T, &In) -> Out, warm: &In) -> usize
    where
        T: Copy,
    {
        let mut best_index = 0usize;
        let mut best_median = u64::MAX;
        for (index, &impl_) in impls.iter().enumerate() {
            let median = self.median_cycles(impl_, run, warm);
            // Strictly-less keeps the earliest index on a tie (determinism).
            if median < best_median {
                best_median = median;
                best_index = index;
            }
        }
        best_index
    }

    /// The median timed-round cycle cost of one implementation over `warm`.
    fn median_cycles<T, In, Out>(&self, impl_: T, run: fn(T, &In) -> Out, warm: &In) -> u64
    where
        T: Copy,
    {
        // `rounds` is clamped to `MAX_ROUNDS` at construction, so a fixed stack
        // buffer holds every sample and the measurement allocates nothing.
        let mut samples = [0u64; MAX_ROUNDS as usize];
        let rounds = self.rounds.min(MAX_ROUNDS) as usize;
        for sample in samples.iter_mut().take(rounds) {
            let start = self.cycles.cycles();
            for _ in 0..self.iters {
                let out = run(impl_, warm);
                core::hint::black_box(out);
            }
            let end = self.cycles.cycles();
            *sample = end.wrapping_sub(start);
        }
        median(&mut samples[..rounds])
    }
}

/// The maximum number of independent measurement rounds.
///
/// The `rounds` budget is clamped to this at construction so a round's samples
/// live on a fixed stack buffer and the microbenchmark allocates nothing — a
/// bounded, one-shot measurement, never a heap-backed growing series. Well
/// above the default round count; a larger request is clamped, not honoured.
const MAX_ROUNDS: u32 = 64;

/// The median of `samples`, sorting in place. Returns `0` for an empty slice
/// (the safe default; callers never measure an empty budget).
fn median(samples: &mut [u64]) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    samples[samples.len() / 2]
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;

    /// A deterministic fake counter whose value only advances when the op
    /// under test charges it, so a test can make one candidate provably
    /// "faster" than another without any real timing.
    struct ScriptedCounter {
        now: Cell<u64>,
    }

    impl ScriptedCounter {
        fn new() -> Self {
            Self { now: Cell::new(0) }
        }
        fn advance(&self, by: u64) {
            self.now.set(self.now.get().wrapping_add(by));
        }
    }

    impl CycleCounter for ScriptedCounter {
        fn cycles(&self) -> u64 {
            self.now.get()
        }
        fn cycles_monotonic_hint(&self) -> bool {
            true
        }
    }

    // The warmed input the op runs over: a (non-`Copy`) handle to the shared
    // counter, mirroring a real consumer whose input is a buffer.
    struct Warm<'a>(&'a ScriptedCounter);

    // The op under test: the impl handle `cost` is the per-call cycle cost the
    // fake counter is charged, so a cheaper candidate genuinely measures fewer
    // cycles.
    fn run_cost(cost: u64, warm: &Warm<'_>) -> u64 {
        warm.0.advance(cost);
        cost
    }

    #[test]
    fn budget_is_bounded_and_clamped() {
        let ctr = ScriptedCounter::new();
        let h = BenchHarness::with_budget(&ctr, 0, 0);
        assert_eq!(h.iters(), 1, "iters clamps to at least one");
        assert_eq!(h.rounds(), 1, "rounds clamps to at least one");
        let d = BenchHarness::new(&ctr);
        assert_eq!(d.iters(), BenchHarness::DEFAULT_ITERS);
        assert_eq!(d.rounds(), BenchHarness::DEFAULT_ROUNDS);
        assert!(d.cycles.cycles_monotonic_hint());
    }

    #[test]
    fn picks_the_cheaper_candidate() {
        let ctr = ScriptedCounter::new();
        let h = BenchHarness::with_budget(&ctr, 4, 5);
        // Candidate 0 costs 10/call, candidate 1 costs 3/call.
        let impls = [10u64, 3u64];
        let warm = Warm(&ctr);
        assert_eq!(h.fastest(&impls, run_cost, &warm), 1);
    }

    #[test]
    fn ties_break_to_earliest() {
        let ctr = ScriptedCounter::new();
        let h = BenchHarness::with_budget(&ctr, 4, 5);
        let impls = [7u64, 7u64];
        let warm = Warm(&ctr);
        assert_eq!(h.fastest(&impls, run_cost, &warm), 0);
    }

    #[test]
    fn median_rejects_a_single_outlier() {
        // Nine rounds, one wildly slow; the median is unaffected.
        let mut s = [5, 5, 5, 5, 5, 5, 5, 5, 1_000_000];
        assert_eq!(median(&mut s), 5);
        assert_eq!(median(&mut []), 0);
    }

    #[test]
    fn empty_impls_yields_zero() {
        let ctr = ScriptedCounter::new();
        let h = BenchHarness::new(&ctr);
        let impls: [u64; 0] = [];
        let warm = Warm(&ctr);
        assert_eq!(h.fastest(&impls, run_cost, &warm), 0);
    }
}
