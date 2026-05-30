//! Architecture hook for the scheduler.
//!
//! The scheduler is architecture-neutral — it never reaches for an APIC, a
//! GIC, a CLINT or `requestAnimationFrame` directly. Every architecture
//! port (Stage 3 of `PLAN.md`) implements [`SchedulerArch`]; the host test
//! binary uses [`TestArch`], gated behind the `test-arch` Cargo feature
//! (`AGENTS.md` §1 — no hacks: production code never carries a fake
//! IPI/timer implementation).
//!
//! [`SchedulerArch`] and [`CpuId`] themselves are defined in the Arch
//! HAL crate `kernel/arch/api` (`AGENTS.md` §17.2) and re-exported here.
//! This module owns only the host test double [`TestArch`]: keeping the
//! HAL trait in `kernel/arch/api` is what lets an architecture port
//! implement it without depending on `kernel/sched` (§17.4).

#[cfg(any(test, feature = "test-arch"))]
use crate::loom_compat::{AtomicU32, AtomicU64, Ordering};

// The scheduler-facing Arch HAL surface ([`CpuId`], [`SchedulerArch`])
// is defined once in `kernel/arch/api` (`AGENTS.md` §17.2) and
// re-exported here so existing `rustos_kernel_sched::{CpuId,
// SchedulerArch}` paths — and the `kernel/core` `KernelArch:
// SchedulerArch` super-trait — keep resolving to the single canonical
// definition (`AGENTS.md` §2.2 — no duplication).
pub use rustos_arch_api::{CpuId, SchedulerArch};

/// In-memory `SchedulerArch` implementation used by host-side tests.
///
/// `TestArch` is the *only* implementation produced by this crate. Real
/// ports live in `kernel/arch/<arch>/` (Stage 3). It is gated behind the
/// `test-arch` Cargo feature; the crate's own dev-dependency self-link
/// enables it automatically for `cargo test`, and the feature is
/// otherwise opt-in.
///
/// # Cooperative model
///
/// The scheduler is deterministic on a single thread of execution: each
/// host test drives it step-by-step. `TestArch` therefore stores the
/// "current CPU" inside an [`AtomicU32`] that the test scaffolding
/// updates via [`Self::set_current_cpu`] when it switches focus between
/// simulated cores. This matches the production invariant — on a real
/// port `current_cpu()` is whatever CPU the calling code happens to be
/// running on — while staying single-threaded so the tests are
/// deterministic and reproducible (`AGENTS.md` §7 — no flaky tests).
#[cfg(any(test, feature = "test-arch"))]
#[derive(Debug, Default)]
pub struct TestArch {
    current: AtomicU32,
    ticks: AtomicU64,
    /// Count of IPIs dispatched to each CPU, indexed by [`CpuId`].
    ///
    /// Bounded to the configured CPU count; out-of-range targets increment
    /// the [`Self::stray_ipis`] counter instead of corrupting memory.
    ipis: alloc::vec::Vec<AtomicU64>,
    stray_ipis: AtomicU64,
}

#[cfg(any(test, feature = "test-arch"))]
impl TestArch {
    /// Build a `TestArch` that can address `cpus` simulated cores.
    ///
    /// Returns `None` for `cpus == 0` because a scheduler with no CPUs is
    /// a configuration bug; the type system surfaces it at construction
    /// rather than via a runtime panic deep inside the dispatch loop.
    #[must_use]
    pub fn new(cpus: u32) -> Option<Self> {
        if cpus == 0 {
            return None;
        }
        let mut ipis = alloc::vec::Vec::with_capacity(cpus as usize);
        for _ in 0..cpus {
            ipis.push(AtomicU64::new(0));
        }
        Some(Self {
            current: AtomicU32::new(0),
            ticks: AtomicU64::new(0),
            ipis,
            stray_ipis: AtomicU64::new(0),
        })
    }

    /// Returns the number of simulated CPUs.
    #[must_use]
    pub fn cpu_count(&self) -> u32 {
        // Bounded at construction to `u32::MAX` via [`Self::new`].
        u32::try_from(self.ipis.len()).unwrap_or(u32::MAX)
    }

    /// Updates which simulated CPU is "currently executing".
    ///
    /// Tests call this between driving the scheduler on different cores so
    /// that `current_cpu()` reflects the active core, mirroring what a
    /// per-CPU register would return on real hardware.
    pub fn set_current_cpu(&self, cpu: CpuId) {
        self.current.store(cpu, Ordering::Relaxed);
    }

    /// Advances the tick counter by `delta`. Tests use this to simulate
    /// timer interrupts.
    pub fn advance_ticks(&self, delta: u64) {
        self.ticks.fetch_add(delta, Ordering::Relaxed);
    }

    /// Returns the number of IPIs ever delivered to `target`.
    #[must_use]
    pub fn ipi_count(&self, target: CpuId) -> u64 {
        self.ipis
            .get(target as usize)
            .map_or(0, |a| a.load(Ordering::Relaxed))
    }

    /// Returns the number of IPIs sent to out-of-range CPUs. Tests assert
    /// this stays zero.
    #[must_use]
    pub fn stray_ipi_count(&self) -> u64 {
        self.stray_ipis.load(Ordering::Relaxed)
    }
}

#[cfg(any(test, feature = "test-arch"))]
impl SchedulerArch for TestArch {
    fn current_cpu(&self) -> CpuId {
        self.current.load(Ordering::Relaxed)
    }

    fn ticks_now(&self) -> u64 {
        self.ticks.load(Ordering::Relaxed)
    }

    fn send_ipi(&self, target: CpuId) {
        match self.ipis.get(target as usize) {
            Some(slot) => {
                slot.fetch_add(1, Ordering::Relaxed);
            }
            None => {
                self.stray_ipis.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_arch_records_ipis() {
        let arch = TestArch::new(4).expect("4 cpus");
        arch.send_ipi(1);
        arch.send_ipi(1);
        arch.send_ipi(3);
        assert_eq!(arch.ipi_count(0), 0);
        assert_eq!(arch.ipi_count(1), 2);
        assert_eq!(arch.ipi_count(3), 1);
        assert_eq!(arch.stray_ipi_count(), 0);
    }

    #[test]
    fn test_arch_stray_ipi_is_recorded_not_a_panic() {
        let arch = TestArch::new(2).expect("2 cpus");
        arch.send_ipi(9);
        assert_eq!(arch.stray_ipi_count(), 1);
    }

    #[test]
    fn test_arch_rejects_zero_cpu_config() {
        assert!(TestArch::new(0).is_none());
    }

    #[test]
    fn test_arch_ticks_are_monotonic() {
        let arch = TestArch::new(1).expect("1 cpu");
        let before = arch.ticks_now();
        arch.advance_ticks(7);
        assert!(arch.ticks_now() >= before + 7);
    }
}
