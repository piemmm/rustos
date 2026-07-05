//! Tickless load-average accounting.
//!
//! [`LoadTracker`] maintains the three classic exponentially-damped
//! run-queue averages (1, 5, and 15 minutes) without a periodic timer:
//! the kernel is tickless, so instead of a 5-second sampling tick the
//! tracker advances lazily at each *observation* (a `sysinfo_introspect`
//! load-average read). The elapsed time since the previous observation is
//! folded in as whole 5-second periods using the closed-form decay
//! `load = load·d^n + active·(1 − d^n)` — the same fixed-point arithmetic
//! Linux's `calc_load_n` uses for `NO_HZ` idle gaps — so a long quiet gap
//! costs one `fixed_power` evaluation, never a loop per missed tick.
//!
//! Between observations the runnable census is not sampled, so the census
//! read *at* the observation stands in for the whole gap. That is the
//! honest tickless analogue of periodic sampling: a fixed-frequency
//! sampling tick armed only for load accounting is exactly the needless
//! interrupt load the tickless mandate forbids.

use core::sync::atomic::{AtomicU64, Ordering};

use rustos_abi::sysinfo::LOAD_FIXED_SHIFT;

/// 1.0 in the fixed-point load representation.
const FIXED_1: u64 = 1 << LOAD_FIXED_SHIFT;

/// Length of one damping period, in nanoseconds (the classic 5 seconds).
const LOAD_FREQ_NS: u64 = 5_000_000_000;

/// Per-period decay factors for the 1/5/15-minute windows:
/// `e^(-5s/window)` in [`LOAD_FIXED_SHIFT`]-bit fixed point.
const EXP: [u64; 3] = [1884, 2014, 2037];

/// `base^n` in `frac_bits` fixed point, by repeated squaring with
/// round-to-nearest at each step (Linux's `fixed_power_int`).
fn fixed_power(mut base: u64, frac_bits: u32, mut n: u64) -> u64 {
    let mut result = 1u64 << frac_bits;
    let half = 1u64 << (frac_bits - 1);
    while n != 0 {
        if n & 1 != 0 {
            result = (result * base + half) >> frac_bits;
        }
        n >>= 1;
        if n == 0 {
            break;
        }
        base = (base * base + half) >> frac_bits;
    }
    result
}

/// One damped average advanced by `periods` whole periods with decay
/// `exp`, toward the census `active` (already in fixed point).
fn advance(load: u64, exp: u64, active: u64, periods: u64) -> u64 {
    // One combined shift (Linux's `calc_load` form): rounding once keeps
    // the fixed point of the recurrence at the census itself, where two
    // separate truncating shifts would bias the steady state low.
    let decay = fixed_power(exp, LOAD_FIXED_SHIFT, periods);
    (load * decay + active * (FIXED_1 - decay) + FIXED_1 / 2) >> LOAD_FIXED_SHIFT
}

/// The three damped run-queue averages, advanced at observation points.
///
/// Lock-free: the loads and the last-advance instant are atomics, so an
/// observation never blocks a syscall path. Concurrent observers race
/// benignly — the single `compare_exchange` on the period boundary lets
/// exactly one advance the averages for a given elapsed window.
pub struct LoadTracker {
    /// The damped averages in [`LOAD_FIXED_SHIFT`]-bit fixed point.
    loads: [AtomicU64; 3],
    /// Monotonic instant (ns) the averages were last advanced to.
    last_advance_ns: AtomicU64,
}

impl LoadTracker {
    /// A tracker at boot: zero load, no periods elapsed.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            loads: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
            last_advance_ns: AtomicU64::new(0),
        }
    }

    /// Fold the time since the last advance into the averages using the
    /// runnable census `runnable`, and return the current fixed-point
    /// `[load1, load5, load15]` (saturated to `u32`, the wire width).
    ///
    /// A monotonic reading that has not crossed a period boundary — or a
    /// clock that appears to run backwards — leaves the averages
    /// untouched and simply reports them.
    pub fn observe(&self, now_ns: u64, runnable: u64) -> [u32; 3] {
        let last = self.last_advance_ns.load(Ordering::Acquire);
        let elapsed = now_ns.saturating_sub(last);
        let periods = elapsed / LOAD_FREQ_NS;
        if periods > 0
            && self
                .last_advance_ns
                .compare_exchange(
                    last,
                    last + periods * LOAD_FREQ_NS,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
        {
            let active = runnable.saturating_mul(FIXED_1);
            for (slot, exp) in self.loads.iter().zip(EXP) {
                let advanced = advance(slot.load(Ordering::Acquire), exp, active, periods);
                slot.store(advanced, Ordering::Release);
            }
        }
        let report =
            |slot: &AtomicU64| u32::try_from(slot.load(Ordering::Acquire)).unwrap_or(u32::MAX);
        [
            report(&self.loads[0]),
            report(&self.loads[1]),
            report(&self.loads[2]),
        ]
    }
}

impl Default for LoadTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{advance, fixed_power, LoadTracker, EXP, FIXED_1, LOAD_FREQ_NS};

    #[test]
    fn fixed_power_matches_iterated_multiplication() {
        // Repeated squaring rounds at different points than the naive
        // product, so the two agree to within a couple of ULPs — never
        // more.
        for n in 0..64u64 {
            let mut expected = FIXED_1;
            for _ in 0..n {
                expected = (expected * EXP[0] + FIXED_1 / 2) >> 11;
            }
            let got = fixed_power(EXP[0], 11, n);
            assert!(got.abs_diff(expected) <= 8, "n = {n}: {got} vs {expected}");
        }
        // n = 0 is the identity for any base.
        assert_eq!(fixed_power(3, 11, 0), FIXED_1);
    }

    #[test]
    fn constant_census_converges_to_the_census() {
        // A steady census of 2 runnable tasks pulls every window toward
        // 2.00 — from below and, symmetrically, from above.
        let active = 2 * FIXED_1;
        for exp in EXP {
            let mut rising = 0u64;
            let mut falling = 8 * FIXED_1;
            for _ in 0..2000 {
                rising = advance(rising, exp, active, 1);
                falling = advance(falling, exp, active, 1);
            }
            // The floor fixpoint of the rounded recurrence sits within
            // (FIXED_1/2)/(FIXED_1 - exp) of the census — the longer the
            // window, the wider the band (about 0.05 task at 15 minutes).
            let tolerance = FIXED_1 / (FIXED_1 - exp) + 2;
            assert!(rising.abs_diff(active) <= tolerance, "rising {rising}");
            assert!(falling.abs_diff(active) <= tolerance, "falling {falling}");
        }
    }

    #[test]
    fn a_long_gap_equals_the_same_periods_taken_singly() {
        // The closed form over n periods must agree with n single steps.
        let active = 3 * FIXED_1;
        for exp in EXP {
            let mut stepped = 5 * FIXED_1;
            for _ in 0..17 {
                stepped = advance(stepped, exp, active, 1);
            }
            let jumped = advance(5 * FIXED_1, exp, active, 17);
            assert!(stepped.abs_diff(jumped) <= 17, "{stepped} vs {jumped}");
        }
    }

    #[test]
    fn observe_advances_only_across_period_boundaries() {
        let tracker = LoadTracker::new();
        // Within the first period nothing advances: the load stays zero
        // even under a large census.
        assert_eq!(tracker.observe(LOAD_FREQ_NS - 1, 100), [0, 0, 0]);
        // Crossing the boundary folds the census in.
        let after = tracker.observe(LOAD_FREQ_NS, 100);
        assert!(after[0] > 0);
        // The 1-minute window reacts faster than the 15-minute one.
        assert!(after[0] > after[2]);
    }

    #[test]
    fn a_backwards_clock_is_a_no_op() {
        let tracker = LoadTracker::new();
        let loads = tracker.observe(10 * LOAD_FREQ_NS, 4);
        assert_eq!(tracker.observe(0, 400), loads);
    }

    #[test]
    fn an_idle_system_decays_toward_zero() {
        let tracker = LoadTracker::new();
        let busy = tracker.observe(LOAD_FREQ_NS, 8);
        assert!(busy[0] > 0);
        // A long idle gap decays every window to nothing.
        let idle = tracker.observe(3 * 3600 * 1_000_000_000, 0);
        assert_eq!(idle, [0, 0, 0]);
    }
}
