//! TAIRiX Arch HAL — the architecture hardware-abstraction layer.
//!
//! `kernel/arch/api` is the closed set of traits that every
//! architecture port (`kernel/arch/<target>`) implements and the rest
//! of the kernel consumes. It is the
//! boundary that keeps the architecture pluggable: a port names only
//! this crate (and `lib/*`), never a concrete kernel subsystem, and
//! no kernel crate names a concrete arch port — both sides meet here.
//!
//! # Scope
//!
//! This crate currently hosts the **scheduler-facing** slice of the
//! HAL — the per-CPU identity, the monotonic tick source, and the
//! inter-processor preemption hook the SMP scheduler drives through
//! [`SchedulerArch`] — the **side-channel mitigation** slice: the [`SideChannelMitigation`] trait and its
//! [`sidechannel::conformance`] vertical — and the **memory-tagging**
//! slice: the [`MemoryTagging`] trait, its
//! [`memtag::conformance`] vertical, and the architecture-neutral
//! [`MemTag`] / [`next_free_tag`] tag algebra that hardens
//! use-after-free — and the **enter-user-mode** slice: the [`EnterUser`] trait and the architecture-neutral
//! [`UserEntry`] register state that drops a freshly built process image
//! into user mode via the port's native transition (`sret` on riscv64,
//! `eret` on aarch64, `iretq` on x86_64) — and the **early-boot
//! platform-discovery** slice: the
//! [`PlatformDiscovery`] trait that normalises a target's native
//! hardware source into the [`tairix_abi::hwtree`] hardware tree, plus
//! its [`platform::conformance`] vertical — and the **per-CPU storage**
//! slice: the [`PerCpu`] trait that reads and writes
//! the calling CPU's per-CPU base word (GS base on x86_64, `TPIDR_EL1`
//! on aarch64, `tp` on riscv64, a per-worker slot on wasm32), plus its
//! [`percpu::conformance`] round-trip + isolation vertical — and the
//! **interrupt entry/exit** slice: the
//! [`IrqController`] line-masking trait and the [`InterruptEntry`]
//! claim/complete prologue/epilogue every claim-based port exposes, plus
//! their [`irq::conformance`] verticals — and the **timer-programming**
//! slice: the [`Timer`] trait that installs the one
//! scheduler-tick callback and dispatches a tick to it (the LAPIC timer,
//! the EL1 generic timer, the SBI timer, and the wasm32
//! `requestAnimationFrame` loop all drive it), plus its
//! [`timer::conformance`] vertical — and the **context-switch** slice: the [`ContextSwitch`] trait that seeds a
//! never-run task's first frame ([`ContextSwitch::prepare`]) and performs
//! the port's native task switch ([`ContextSwitch::switch`]) over the
//! architecture-neutral [`TaskContext`] save area, plus its
//! [`context::conformance`] vertical — and the **MMU/page-table** slice: the [`AddressSpace`] trait that installs a 4 KiB
//! mapping ([`AddressSpace::map_page`]), reports the root-table physical
//! address ([`AddressSpace::root_phys`]), and activates the translation
//! regime ([`AddressSpace::activate`]) over the neutral [`PageFlags`]
//! permission set, plus its [`mmu::conformance`] vertical — and the
//! **TLB-shootdown** slice: the [`TlbShootdown`]
//! trait whose [`TlbShootdown::flush_page`] the per-process map/unmap
//! path drives to invalidate one CPU's stale cached translation
//! (`invlpg` / `tlbi vae1is` / `sfence.vma`), plus its
//! [`tlb::conformance`] vertical — the MMU slice also carries the
//! **fault-guarded user-copy** extension ([`uaccess`]): the set-once
//! [`GuardedCopyFn`] slot each port's fault-windowed span copy is
//! published through, so a hardware fault taken mid user-copy resumes at
//! the window's fix-up and surfaces as an error instead of the fatal
//! path, plus its [`uaccess::conformance`] checks — and the **cross-CPU TLB-shootdown**
//! slice (`plans/WIRING.md` W6): the
//! [`CrossCpuTlbShootdown`] trait whose [`CrossCpuTlbShootdown::shootdown_page`]
//! invalidates a stale translation on *every* online CPU (an x86_64
//! IPI + acknowledge, an aarch64 inner-shareable `tlbi ...is` broadcast,
//! a riscv64 SBI `remote_sfence_vma`), plus its [`xtlb::conformance`]
//! vertical — and the **page-table frame-source**
//! slice (`plans/WIRING.md` W5b-3): the
//! [`PageTableFrames`] trait a port draws its root and intermediate
//! tables from (as [`TableFrame`]s), so a real per-process address space
//! is backed by the `kernel/mem` frame allocator while the static
//! `PageTablePool` stays the boot/bootstrap source, plus its
//! [`frames::conformance`] vertical — and the **SMP secondary-CPU
//! bring-up** slice (`plans/WIRING.md` W14): the
//! [`SecondaryBringup`] trait whose [`SecondaryBringup::start_secondary`]
//! starts a parked logical CPU (an x86_64 INIT-SIPI-SIPI handshake, an
//! aarch64 PSCI `CPU_ON`, a riscv64 SBI HSM `hart_start`, a wasm32 Web
//! Worker spawn), plus its [`smp::conformance`] vertical — and the
//! **machine-takeover** slice (`plans/NEW-SUPERVISOR.md` §9): the
//! [`MachineTakeover`] trait whose single [`MachineTakeover::take_over`]
//! operation hands the whole machine over to the pre-boot Supervisor's
//! one-way whole-RAM test — quiesce the other CPUs, mask
//! interrupts, stop the watchdog, flatten paging, run the sweep on a reserved
//! stack, then reset (it never returns on success) — plus its
//! [`takeover::conformance`] vertical. It also hosts
//! the ** conformance
//! vertical** ([`conformance`]): the harness every port runs over its
//! real HAL handles so parity is *enforced* rather than asserted by
//! inspection (`plans/WIRING.md`). The remaining HAL surface enumerated
//! by is now complete: the last ad-hoc slice — SMP
//! secondary-CPU bring-up — became the [`SecondaryBringup`] trait in
//! Stage W14 (`plans/WIRING.md`). Until a future primitive lives here it
//! stays in its current owning crate, and the move is tracked, not
//! silently duplicated.
//!
//! It also hosts the **CPU-feature-detection** slice: the [`CpuFeatures`]
//! trait that turns each target's CPU-ID source (`CPUID`,
//! `ID_AA64ISAR0_EL1`/`ID_AA64PFR0_EL1`, `misa` + the device-tree ISA
//! string, the wasm32 host query) into one arch-neutral [`CpuFeatureSet`]
//! and [`CoreType`] key — the deterministic capability layer the
//! `lib/cpuops` self-optimising dispatch framework
//! (`plans/FIX-HARDWARE-FEATURES.md`) gates on — plus its
//! [`cpufeatures::conformance`] vertical, and the **CPU cycle-counter**
//! slice: the [`CpuCycles`] trait that reads the per-core cycle count
//! (`rdtsc` / `PMCCNTR_EL0` / `CNTVCT_EL0` / `rdcycle` /
//! `performance.now()`) the `lib/cpuops` microbenchmark measures over,
//! plus its [`cpucycles::conformance`] vertical.
//!
//! # Why `no_std` and dependency-light
//!
//! the charter permits `kernel/arch/api` to depend on `lib/*` only. The crate
//! is `no_std` and names a single `lib/*` dependency — `tairix_abi`,
//! itself `no_std`, dependency-free, and allocator-free — so the
//! [`PlatformDiscovery`] slice can speak in the one hardware-tree ABI
//! rather than re-defining it. An architecture port
//! implementing the HAL therefore still acquires no transitive edge to a
//! concrete kernel crate, and the architecture-neutral kernel can name
//! the HAL without inheriting an arch dependency.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

// The device-tree fixture builder the `fdtwalk` tests compose their trees
// with returns owned blobs, and the collecting sink they walk into grows.
// Test scaffolding only; the crate itself stays allocator-free.
#[cfg(test)]
extern crate std;

pub mod backtrace;
pub mod conformance;
pub mod context;
pub mod coreclock;
pub mod cpucycles;
pub mod cpufeatures;
pub mod entropy;
pub mod fdtwalk;
pub mod frames;
pub mod irq;
pub mod memtag;
pub mod mmu;
pub mod percpu;
pub mod platform;
pub mod quiesce;
pub mod sidechannel;
pub mod smp;
pub mod takeover;
pub mod timer;
pub mod tlb;
pub mod uaccess;
pub mod userentry;
pub mod wakeup;
pub mod watchdog;
pub mod xtlb;

pub use backtrace::{
    conformance as backtrace_conformance, walk as backtrace_walk, Backtrace, BacktraceEntry,
    BacktraceProfile, CpuStateCapture, FrameLayout, NamedReg, RegisterSnapshot, StackBounds,
    StackReader, Translation, MAX_FRAMES as BACKTRACE_MAX_FRAMES, MAX_NAMED_REGS, MAX_TABLE_LEVELS,
};

pub use sidechannel::{
    conformance as sidechannel_conformance, Mitigation, MitigationEntry, MitigationProfile,
    ProfileError, SideChannelMitigation,
};

pub use cpufeatures::{
    conformance as cpufeatures_conformance, CoreType, CpuFeature, CpuFeatureSet, CpuFeatures,
    FeatureEntry, FeatureProfile, FeatureSupport,
};

pub use cpucycles::{conformance as cpucycles_conformance, CpuCycles};

pub use coreclock::{
    conformance as coreclock_conformance, frequency_hz, CoreClock, CoreClockSupport,
};

pub use entropy::{
    conformance as entropy_conformance, EntropyEntry, EntropyProfile, EntropySupport,
    PlatformEntropy,
};
pub use memtag::{
    conformance as memtag_conformance, next_free_tag, MemTag, MemoryTagging, Tagging, TaggingEntry,
    TaggingProfile, TAG_COUNT,
};

pub use userentry::{EnterUser, UserEntry};

pub use platform::{
    conformance as platform_conformance, DiscoveryError, HwNodeSink, PlatformDiscovery,
};

pub use percpu::{conformance as percpu_conformance, PerCpu};

pub use irq::{conformance as irq_conformance, InterruptEntry, IrqControlError, IrqController};

pub use timer::{conformance as timer_conformance, TickFn, Timer};

pub use context::{
    conformance as context_conformance, ContextSwitch, PrepareError, TaskContext, TaskEntry,
};

pub use mmu::{
    conformance as mmu_conformance, AddressSpace, BlockSplit, KernelWindow, MapError, PageFlags,
};

pub use frames::{
    conformance as frames_conformance, reclaim_hierarchy, PageTableFrames, TableFrame,
    PAGE_TABLE_ENTRIES,
};

pub use tlb::{conformance as tlb_conformance, TlbShootdown};

pub use uaccess::{
    conformance as uaccess_conformance, copy_user_span, install_guarded_copy, pc_in_window,
    CopySpanFault, GuardedCopyFn, InstallGuardedCopyError,
};

pub use xtlb::{conformance as xtlb_conformance, CrossCpuTlbShootdown};

pub use smp::{conformance as smp_conformance, SecondaryBringup, SmpError};

pub use takeover::{conformance as takeover_conformance, MachineTakeover, TakeoverError};

pub use quiesce::{
    acknowledge as quiesce_acknowledge, publish_tables as quiesce_publish_tables, quiesce_others,
    stop_others_best_effort as quiesce_stop_others_best_effort,
    stop_requested as quiesce_stop_requested, PublishError as QuiescePublishError,
    StopOutcome as QuiesceStopOutcome,
};

pub use watchdog::{
    conformance as watchdog_conformance, InFlightInterrupt, RecoveryOutcome, RemotePcSample,
    StuckInterrupt, WatchdogArch, WatchdogKind, WatchdogSample, CADENCE_NS as WATCHDOG_CADENCE_NS,
};

/// Identifier for a logical CPU (hardware thread) the kernel manages.
///
/// Stable for the lifetime of the kernel image. Architecture ports map
/// these to APIC IDs / MPIDR / hart IDs / worker indices in their boot
/// code; the architecture-neutral kernel treats them as opaque dense
/// indices into its per-CPU arrays.
pub type CpuId = u32;

/// The performance class of a logical CPU on a heterogeneous machine.
///
/// Modern asymmetric CPUs (Intel "hybrid" / `big.LITTLE` / `DynamIQ`)
/// pair high-throughput **performance** cores with low-power
/// **efficiency** cores. The class is a *static identity* property of a
/// [`CpuId`] — like [`SchedulerArch::current_cpu`] it never changes for
/// the lifetime of the kernel image — discovered by the architecture
/// port during early-boot platform enumeration. It is deliberately distinct from *dynamic* power management
/// (frequency scaling, deep sleep), which is not part of this surface.
///
/// The scheduler uses it to place latency-insensitive background work on
/// efficiency cores and to migrate a task to a performance core when it
/// needs throughput (`docs/src/architecture/scheduler.md`). A homogeneous
/// machine reports every CPU as [`CoreClass::Performance`]; the rest of
/// the kernel then treats placement as a no-op.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Default)]
pub enum CoreClass {
    /// A high-throughput core (Intel "Core" / ARM "big"). The default
    /// on a homogeneous machine.
    #[default]
    Performance = 0,
    /// A low-power core (Intel "Atom" / ARM "LITTLE"), preferred for
    /// idle/background work.
    Efficiency = 1,
}

impl CoreClass {
    /// Returns the raw discriminant as stored in an atomic.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Inverse of [`Self::as_u8`]; returns `None` for unknown encodings.
    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Performance),
            1 => Some(Self::Efficiency),
            _ => None,
        }
    }

    /// `true` for [`CoreClass::Performance`].
    #[must_use]
    pub const fn is_performance(self) -> bool {
        matches!(self, Self::Performance)
    }

    /// `true` for [`CoreClass::Efficiency`].
    #[must_use]
    pub const fn is_efficiency(self) -> bool {
        matches!(self, Self::Efficiency)
    }
}

/// Architecture surface the SMP scheduler needs to drive a system.
///
/// Every architecture port implements this trait; the host test
/// double `TestArch` (shipped by `kernel/sched` behind its `test-arch`
/// feature) is the only non-port implementation in the workspace
/// (production code never carries a fake IPI/timer).
///
/// Implementations must be both [`Send`] and [`Sync`] because the
/// scheduler stores them inside `Arc`s shared between every CPU.
///
/// # Required semantics
///
/// * [`Self::current_cpu`] must return the calling CPU's [`CpuId`]. On
///   a real port this comes from a per-CPU register or an APIC read;
///   the value must be stable for the duration of the call.
/// * [`Self::ticks_now`] returns a monotonically non-decreasing tick
///   counter. The unit is arbitrary but consistent within a single
///   port (typically 1 ms or one timer tick). Wraparound at `u64::MAX`
///   is permitted but not expected in any realistic kernel uptime.
/// * [`Self::send_ipi`] must arrange for the target CPU to enter the
///   scheduler's preemption entry point "soon" — the exact latency is
///   port-defined. Sending an IPI to the calling CPU is allowed and is
///   a no-op equivalent to setting a self-reschedule flag.
///
/// The trait is deliberately tiny. *Dynamic* power management (per-core
/// timer programming, deep sleep, frequency scaling) belongs in the
/// arch crate itself, not in this surface; growing the trait with that
/// would constitute interface creep. [`Self::core_class`]
/// is admitted because it is *static* per-CPU identity — the same
/// category as [`Self::current_cpu`] — that the architecture-neutral
/// scheduler genuinely needs to place work, and it is a provided method
/// so existing ports inherit the homogeneous default unchanged.
pub trait SchedulerArch: Send + Sync {
    /// Returns the calling CPU's identifier.
    fn current_cpu(&self) -> CpuId;

    /// Returns the current monotonic tick.
    fn ticks_now(&self) -> u64;

    /// Asks `target` to enter the scheduler at its next safe point.
    ///
    /// Real ports raise a hardware IPI; the host-side `TestArch`
    /// records the request in an in-memory ledger so host tests can
    /// assert that preemption was requested.
    fn send_ipi(&self, target: CpuId);

    /// Returns the static [`CoreClass`] of `cpu`.
    ///
    /// The default treats every CPU as a [`CoreClass::Performance`]
    /// core — the correct answer for a homogeneous machine and for any
    /// port that has not yet wired heterogeneous-core discovery. A port
    /// on asymmetric hardware (Intel hybrid, ARM `big.LITTLE`) overrides
    /// this with the class it discovered at boot. An out-of-range `cpu`
    /// must return [`CoreClass::Performance`] (the safe default), never
    /// panic.
    fn core_class(&self, _cpu: CpuId) -> CoreClass {
        CoreClass::Performance
    }

    /// Arm or disarm the **calling** CPU's one-shot preemption timer for
    /// the task the scheduler is about to run.
    ///
    /// The scheduler calls this from its dispatch path on the CPU it is
    /// dispatching to: `armed = true` when that CPU still has at least one
    /// *other* ready task (a competitor that must get a turn), so the
    /// running task is bounded to one scheduling quantum; `armed = false`
    /// when the CPU has nothing to preempt to (it is idle or runs a single
    /// runnable task), so the timer is stopped and the core takes **no**
    /// timer interrupts at all. This is what makes TAIRiX tickless
    /// (`NO_HZ`): the timer is armed one-shot only when a
    /// CPU is contended, never at a fixed frequency.
    ///
    /// It belongs on this surface for the same reason [`Self::send_ipi`]
    /// does — both are *scheduler-asks-arch* per-CPU requests — and the
    /// quantum length (a counter-tick value the port derives from its
    /// discovered timer frequency) lives in the port, so this signal stays
    /// a pure boolean and the arithmetic is never duplicated into the
    /// architecture-neutral scheduler. It is a
    /// **provided** method defaulting to a no-op so the host `TestArch`
    /// and any non-preemptive port inherit the cooperative behaviour
    /// unchanged; a real port overrides it to program its
    /// [`crate::Timer`] one-shot ([`crate::Timer::arm_oneshot`] /
    /// [`crate::Timer::disarm`]). An implementation must never panic.
    fn set_preemption(&self, _armed: bool) {}

    /// Arm (or clear) the **calling** CPU's one-shot to also fire no later
    /// than the absolute monotonic-nanoseconds deadline `deadline_ns`, for
    /// the nearest pending timed blocking wait.
    ///
    /// This is the timed half of the tickless one-shot (:
    /// the timer is armed "to the next event the scheduler actually needs —
    /// the running task's preemption deadline *or* the nearest armed
    /// wakeup"). A blocking wait with a finite timeout (the wait-queue in
    /// `kernel/core`, whose first consumer is `hw_tree_wait`) records its
    /// soonest waiter deadline through this hook so the parked waiter is
    /// woken on time even when the CPU has *no* runnable task to preempt and
    /// would otherwise take no timer interrupt at all.
    ///
    /// `Some(deadline_ns)` requests a wake at or before that monotonic-ns
    /// instant (the same clock [`crate::SchedulerArch`]'s host double and
    /// the kernel's `monotonic_ns` use); `None` clears the timed arming.
    /// It composes with [`Self::set_preemption`]: a port programs its single
    /// physical one-shot to the *earlier* of the quantum arming and this
    /// wakeup, so neither suppresses the other. A deadline already in the
    /// past arms the soonest possible tick rather than wrapping (fail closed).
    ///
    /// **Provided**, defaulting to a no-op so the host `TestArch` and any
    /// non-preemptive port inherit cooperative behaviour unchanged; a real
    /// port overrides it to reprogram its [`crate::Timer`] one-shot. An
    /// implementation must never panic.
    fn set_wakeup(&self, _deadline_ns: Option<u64>) {}
}

/// Convert a tick span from a counter running at `hz` into nanoseconds.
///
/// The one definition of the `ticks * 1e9 / hz` conversion the ports whose
/// tick source is a raw frequency-known counter (aarch64 `CNTPCT`, riscv64
/// `time`) build both their `monotonic_ns` and their tick-span conversion
/// on, so the tick source and the conversion factor can never diverge.
/// The 128-bit intermediate cannot overflow for any realistic uptime, and
/// the `max(1)` defends a malformed frequency from a division trap; a
/// result beyond `u64` saturates rather than wrapping.
#[must_use]
pub fn ticks_to_ns(ticks: u64, hz: u64) -> u64 {
    let ns = u128::from(ticks).saturating_mul(1_000_000_000) / u128::from(hz.max(1));
    u64::try_from(ns).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trivial in-test implementation: the HAL trait carries no
    /// logic of its own, so the contract worth pinning is that it is
    /// object-safe and usable behind a shared reference the way the
    /// scheduler stores it.
    #[derive(Default)]
    struct StubArch {
        cpu: CpuId,
    }

    impl SchedulerArch for StubArch {
        fn current_cpu(&self) -> CpuId {
            self.cpu
        }

        fn ticks_now(&self) -> u64 {
            0
        }

        fn send_ipi(&self, _target: CpuId) {}
    }

    #[test]
    fn scheduler_arch_is_object_safe() {
        let arch = StubArch { cpu: 3 };
        let dynamic: &dyn SchedulerArch = &arch;
        assert_eq!(dynamic.current_cpu(), 3);
        assert_eq!(dynamic.ticks_now(), 0);
        dynamic.send_ipi(0);
    }

    #[test]
    fn core_class_defaults_to_performance_for_homogeneous_ports() {
        // A port that does not override `core_class` reports a
        // homogeneous machine: every CPU is a performance core.
        let arch = StubArch { cpu: 0 };
        let dynamic: &dyn SchedulerArch = &arch;
        assert_eq!(dynamic.core_class(0), CoreClass::Performance);
        assert_eq!(dynamic.core_class(7), CoreClass::Performance);
    }

    #[test]
    fn core_class_round_trips_through_u8() {
        for c in [CoreClass::Performance, CoreClass::Efficiency] {
            assert_eq!(CoreClass::from_u8(c.as_u8()), Some(c));
        }
        assert_eq!(CoreClass::from_u8(2), None);
        assert_eq!(CoreClass::default(), CoreClass::Performance);
        assert!(CoreClass::Performance.is_performance());
        assert!(CoreClass::Efficiency.is_efficiency());
    }
}
