//! Architecture hook for the scheduler, plus the host test double.
//!
//! A scheduler is architecture-neutral — it never reaches for an APIC, a
//! GIC, a CLINT or `requestAnimationFrame` directly. Every architecture
//! port implements [`SchedulerArch`]; the host conformance suite and the
//! implementations' unit tests use [`TestArch`], gated behind the
//! `test-arch` Cargo feature (`AGENTS.md` §1 — no hacks: production code
//! never carries a fake IPI/timer implementation).
//!
//! [`SchedulerArch`] and [`CpuId`] are defined in the Arch HAL crate
//! `kernel/arch/api` (`AGENTS.md` §17.2) and re-exported here. Keeping the
//! HAL trait in `kernel/arch/api` is what lets an architecture port
//! implement it without depending on a scheduler crate (§17.4).

// The scheduler-facing Arch HAL surface ([`CpuId`], [`SchedulerArch`]) is
// defined once in `kernel/arch/api` (`AGENTS.md` §17.2) and re-exported
// here so the scheduler contract, every `kernel/sched/<impl>`, and
// `kernel/core` all name the single canonical definition (`AGENTS.md`
// §2.2 — no duplication).
pub use rustos_arch_api::{CoreClass, CpuId, SchedulerArch};

#[cfg(any(test, feature = "test-arch"))]
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};

/// In-memory [`SchedulerArch`] implementation used by host-side tests and
/// the `conformance` suite.
///
/// `TestArch` is the *only* implementation produced by this crate. Real
/// ports live in `kernel/arch/<arch>/`. It is gated behind the
/// `test-arch` Cargo feature.
///
/// # Cooperative model
///
/// A scheduler is deterministic on a single thread of execution: each
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
    /// Static [`CoreClass`] of each simulated CPU, indexed by [`CpuId`].
    ///
    /// Initialised to [`CoreClass::Performance`] (a homogeneous machine)
    /// and mutated by [`Self::set_core_class`] so a host test can model
    /// an asymmetric (performance + efficiency) topology and assert the
    /// scheduler places work sensibly across it.
    core_classes: alloc::vec::Vec<AtomicU8>,
    /// Last value passed to [`SchedulerArch::set_preemption`] (`1` armed,
    /// `0` disarmed; `2` = never called), plus a count of each, so a host
    /// test can assert the tickless arm/disarm decision without a real
    /// timer.
    last_preemption: AtomicU8,
    arm_count: AtomicU64,
    disarm_count: AtomicU64,
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
        let mut core_classes = alloc::vec::Vec::with_capacity(cpus as usize);
        for _ in 0..cpus {
            core_classes.push(AtomicU8::new(CoreClass::Performance.as_u8()));
        }
        Some(Self {
            current: AtomicU32::new(0),
            ticks: AtomicU64::new(0),
            ipis,
            stray_ipis: AtomicU64::new(0),
            core_classes,
            last_preemption: AtomicU8::new(2),
            arm_count: AtomicU64::new(0),
            disarm_count: AtomicU64::new(0),
        })
    }

    /// Sets the simulated [`CoreClass`] of `cpu`.
    ///
    /// Lets a host test model an asymmetric machine. An out-of-range
    /// `cpu` is a silent no-op (the same fail-soft policy the IPI ledger
    /// uses) so a test cannot panic the scheduler through this path.
    pub fn set_core_class(&self, cpu: CpuId, class: CoreClass) {
        if let Some(slot) = self.core_classes.get(cpu as usize) {
            slot.store(class.as_u8(), Ordering::Relaxed);
        }
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

    /// The most recent [`SchedulerArch::set_preemption`] decision:
    /// `Some(true)` armed, `Some(false)` disarmed, `None` never called.
    /// Lets a host test assert the tickless arm/disarm behaviour.
    #[must_use]
    pub fn last_preemption(&self) -> Option<bool> {
        match self.last_preemption.load(Ordering::Relaxed) {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    /// Number of times preemption was armed (`set_preemption(true)`).
    #[must_use]
    pub fn arm_count(&self) -> u64 {
        self.arm_count.load(Ordering::Relaxed)
    }

    /// Number of times preemption was disarmed (`set_preemption(false)`).
    #[must_use]
    pub fn disarm_count(&self) -> u64 {
        self.disarm_count.load(Ordering::Relaxed)
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

    fn core_class(&self, cpu: CpuId) -> CoreClass {
        // Out-of-range CPUs report the safe default per the trait
        // contract; a stored byte is always a valid encoding because
        // `set_core_class` only ever writes `CoreClass::as_u8`.
        self.core_classes
            .get(cpu as usize)
            .map_or(CoreClass::Performance, |slot| {
                CoreClass::from_u8(slot.load(Ordering::Relaxed)).unwrap_or(CoreClass::Performance)
            })
    }

    fn set_preemption(&self, armed: bool) {
        self.last_preemption
            .store(u8::from(armed), Ordering::Relaxed);
        if armed {
            self.arm_count.fetch_add(1, Ordering::Relaxed);
        } else {
            self.disarm_count.fetch_add(1, Ordering::Relaxed);
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
