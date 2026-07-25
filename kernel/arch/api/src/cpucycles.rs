//! CPU cycle-counter surface of the Arch HAL.
//!
//! The `lib/cpuops` microbenchmark
//! (`plans/FIX-HARDWARE-FEATURES.md` P3) decides, once per boot per
//! distinct core type, which of several equally-correct implementations
//! of an accelerated operation is *fastest* on this silicon. That needs
//! a cheap, high-resolution per-core cycle count, and reading it is
//! target-divergent (`rdtsc` on x86_64, `PMCCNTR_EL0` / `CNTVCT_EL0` on
//! aarch64, `rdcycle` / `time` on riscv64, `performance.now()` on the
//! wasm32 host), so it is a closed Arch HAL slice.
//!
//! It is deliberately *its own* tiny slice rather than a verb grafted
//! onto the [`super::timer`] scheduler-tick surface: the timer installs
//! the one preemption/wakeup callback and must not grow a benchmarking
//! method (that would be interface creep). The two share nothing but the
//! underlying counter hardware, which each port already owns.
//!
//! # What lives here
//!
//! * [`CpuCycles`] — the per-port handle the benchmark harness reads
//!   through, plus a monotonicity hint.
//! * [`conformance`] — the conformance vertical every port runs: the
//!   counter is non-decreasing across a short busy window.

/// The cycle-counter handle an architecture port exposes.
///
/// Read by the `lib/cpuops` benchmark harness (never on a hot path);
/// production kernel code times nothing else through it. Implementations
/// must be [`Send`] + [`Sync`]: the harness runs on each CPU as it comes
/// up.
pub trait CpuCycles: Send + Sync {
    /// The current value of the calling CPU's cycle counter.
    ///
    /// The unit is architecture-defined (core clock cycles for a PMU
    /// counter, a fixed-rate reference tick for `CNTVCT_EL0`); the
    /// harness only ever compares *deltas* on one core, so the absolute
    /// scale is irrelevant. The value must be non-decreasing for the
    /// duration of a measurement on a single core (see
    /// [`Self::cycles_monotonic_hint`]).
    fn cpu_cycles(&self) -> u64;

    /// `true` if [`Self::cpu_cycles`] is a *reliable* monotonic,
    /// constant-rate time base on this port (an Invariant TSC, a
    /// PMU cycle counter enabled at EL1, the architected generic timer).
    ///
    /// `false` warns the harness that the counter may drift or vary in
    /// rate (a non-invariant TSC, a host stub), so a measurement should
    /// be treated with more caution — but it is still non-decreasing and
    /// usable for a bounded one-shot comparison. It is a hint, not a
    /// gate; the harness never *fails* on it.
    fn cycles_monotonic_hint(&self) -> bool;
}

/// The cycle-counter conformance vertical.
///
/// Every architecture port runs [`conformance::run_all`] against its
/// [`CpuCycles`] handle. Portable and host-run, exactly like the
/// [`super::memtag`] vertical.
pub mod conformance {
    use super::CpuCycles;

    /// Run the entire cycle-counter conformance suite against `port`.
    ///
    /// # Panics
    ///
    /// Panics (failing the test) if the counter runs backwards across a
    /// short busy window — the one property every consumer relies on.
    pub fn run_all<C: CpuCycles + ?Sized>(port: &C) {
        counter_is_non_decreasing(port);
    }

    /// The counter never goes backwards across a short window of reads.
    ///
    /// A constant counter (a host stub that has no real cycle source)
    /// trivially satisfies this — non-decreasing, never *strictly*
    /// increasing is permitted, because the hint reports whether the
    /// source is a real time base.
    fn counter_is_non_decreasing<C: CpuCycles + ?Sized>(port: &C) {
        let mut last = port.cpu_cycles();
        for _ in 0..64 {
            // A little work between reads so a real counter advances.
            core::hint::spin_loop();
            let now = port.cpu_cycles();
            assert!(
                now >= last,
                "cpu_cycles went backwards: {now} < {last} (must be non-decreasing on one core)"
            );
            last = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU64, Ordering};

    /// A monotonic counter that advances one tick per read — a faithful
    /// stub of a real cycle source.
    #[derive(Default)]
    struct MonotonicStub {
        ticks: AtomicU64,
    }

    impl CpuCycles for MonotonicStub {
        fn cpu_cycles(&self) -> u64 {
            self.ticks.fetch_add(1, Ordering::Relaxed) + 1
        }
        fn cycles_monotonic_hint(&self) -> bool {
            true
        }
    }

    /// A constant counter — the honest host stub for a port with no real
    /// cycle source. Still non-decreasing.
    struct ConstantStub;

    impl CpuCycles for ConstantStub {
        fn cpu_cycles(&self) -> u64 {
            42
        }
        fn cycles_monotonic_hint(&self) -> bool {
            false
        }
    }

    #[test]
    fn conformance_accepts_a_monotonic_counter() {
        let port = MonotonicStub::default();
        conformance::run_all(&port);
        let dynamic: &dyn CpuCycles = &port;
        conformance::run_all(dynamic);
        assert!(port.cycles_monotonic_hint());
    }

    #[test]
    fn conformance_accepts_a_constant_counter() {
        conformance::run_all(&ConstantStub);
        assert!(!ConstantStub.cycles_monotonic_hint());
    }

    /// A counter that runs backwards must be rejected.
    struct BackwardsStub;

    impl CpuCycles for BackwardsStub {
        fn cpu_cycles(&self) -> u64 {
            static FIRST: AtomicU64 = AtomicU64::new(0);
            if FIRST.swap(1, Ordering::Relaxed) == 0 {
                100
            } else {
                1
            }
        }
        fn cycles_monotonic_hint(&self) -> bool {
            true
        }
    }

    #[test]
    #[should_panic(expected = "cpu_cycles went backwards")]
    fn conformance_rejects_a_backwards_counter() {
        conformance::run_all(&BackwardsStub);
    }
}
