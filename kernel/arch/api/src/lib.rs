//! RustOS Arch HAL — the architecture hardware-abstraction layer.
//!
//! `kernel/arch/api` is the closed set of traits that every
//! architecture port (`kernel/arch/<target>`) implements and the rest
//! of the kernel consumes (`AGENTS.md` §17.2). It is the §17.4
//! boundary that keeps the architecture pluggable: a port names only
//! this crate (and `lib/*`), never a concrete kernel subsystem, and
//! no kernel crate names a concrete arch port — both sides meet here.
//!
//! # Scope
//!
//! This crate currently hosts the **scheduler-facing** slice of the
//! HAL — the per-CPU identity, the monotonic tick source, and the
//! inter-processor preemption hook the SMP scheduler drives through
//! [`SchedulerArch`] — the **side-channel mitigation** slice
//! (`AGENTS.md` §19.1): the [`SideChannelMitigation`] trait and its
//! [`sidechannel::conformance`] vertical — and the **memory-tagging**
//! slice (`AGENTS.md` §19.10): the [`MemoryTagging`] trait, its
//! [`memtag::conformance`] vertical, and the architecture-neutral
//! [`MemTag`] / [`next_free_tag`] tag algebra that hardens
//! use-after-free — and the **enter-user-mode** slice (`AGENTS.md`
//! §17.2): the [`EnterUser`] trait and the architecture-neutral
//! [`UserEntry`] register state that drops a freshly built process image
//! into user mode via the port's native transition (`sret` on riscv64,
//! `eret` on aarch64, `iretq` on x86_64) — and the **early-boot
//! platform-discovery** slice (`AGENTS.md` §17.2 / §18.1/§18.2): the
//! [`PlatformDiscovery`] trait that normalises a target's native
//! hardware source into the [`rustos_abi::hwtree`] hardware tree, plus
//! its [`platform::conformance`] vertical — and the **per-CPU storage**
//! slice (`AGENTS.md` §17.2): the [`PerCpu`] trait that reads and writes
//! the calling CPU's per-CPU base word (GS base on x86_64, `TPIDR_EL1`
//! on aarch64, `tp` on riscv64, a per-worker slot on wasm32), plus its
//! [`percpu::conformance`] round-trip + isolation vertical — and the
//! **interrupt entry/exit** slice (`AGENTS.md` §17.2): the
//! [`IrqController`] line-masking trait and the [`InterruptEntry`]
//! claim/complete prologue/epilogue every claim-based port exposes, plus
//! their [`irq::conformance`] verticals — and the **timer-programming**
//! slice (`AGENTS.md` §17.2): the [`Timer`] trait that installs the one
//! scheduler-tick callback and dispatches a tick to it (the LAPIC timer,
//! the EL1 generic timer, the SBI timer, and the wasm32
//! `requestAnimationFrame` loop all drive it), plus its
//! [`timer::conformance`] vertical — and the **context-switch** slice
//! (`AGENTS.md` §17.2): the [`ContextSwitch`] trait that seeds a
//! never-run task's first frame ([`ContextSwitch::prepare`]) and performs
//! the port's native task switch ([`ContextSwitch::switch`]) over the
//! architecture-neutral [`TaskContext`] save area, plus its
//! [`context::conformance`] vertical — and the **MMU/page-table** slice
//! (`AGENTS.md` §17.2): the [`AddressSpace`] trait that installs a 4 KiB
//! mapping ([`AddressSpace::map_page`]), reports the root-table physical
//! address ([`AddressSpace::root_phys`]), and activates the translation
//! regime ([`AddressSpace::activate`]) over the neutral [`PageFlags`]
//! permission set, plus its [`mmu::conformance`] vertical — and the
//! **TLB-shootdown** slice (`AGENTS.md` §17.2): the [`TlbShootdown`]
//! trait whose [`TlbShootdown::flush_page`] the per-process map/unmap
//! path drives to invalidate one CPU's stale cached translation
//! (`invlpg` / `tlbi vae1is` / `sfence.vma`), plus its
//! [`tlb::conformance`] vertical — and the **cross-CPU TLB-shootdown**
//! slice (`AGENTS.md` §17.2, `plans/WIRING.md` W6): the
//! [`CrossCpuTlbShootdown`] trait whose [`CrossCpuTlbShootdown::shootdown_page`]
//! invalidates a stale translation on *every* online CPU (an x86_64
//! IPI + acknowledge, an aarch64 inner-shareable `tlbi ...is` broadcast,
//! a riscv64 SBI `remote_sfence_vma`), plus its [`xtlb::conformance`]
//! vertical — and the **page-table frame-source**
//! slice (`AGENTS.md` §17.2, `plans/WIRING.md` W5b-3): the
//! [`PageTableFrames`] trait a port draws its root and intermediate
//! tables from (as [`TableFrame`]s), so a real per-process address space
//! is backed by the `kernel/mem` frame allocator while the static
//! `PageTablePool` stays the boot/bootstrap source, plus its
//! [`frames::conformance`] vertical. It also hosts the **§17.2 conformance
//! vertical** ([`conformance`]): the harness every port runs over its
//! real HAL handles so parity is *enforced* rather than asserted by
//! inspection (`plans/WIRING.md`). The remaining HAL surface enumerated
//! by `AGENTS.md` §17.2 — SMP secondary-core bring-up (landed port-side
//! per arch in Stage W6/W8; an `Smp` HAL trait remains a future §17.2
//! decision) — is migrated here as the §17 burn-down advances; see
//! `PLAN.md` / `plans/WIRING.md`. Until a primitive lives here it stays in
//! its current owning crate, and the move is tracked, not silently
//! duplicated (`AGENTS.md` §2.2).
//!
//! # Why `no_std` and dependency-light
//!
//! §17.4 permits `kernel/arch/api` to depend on `lib/*` only. The crate
//! is `no_std` and names a single `lib/*` dependency — `rustos_abi`,
//! itself `no_std`, dependency-free, and allocator-free — so the
//! [`PlatformDiscovery`] slice can speak in the one hardware-tree ABI
//! rather than re-defining it (`AGENTS.md` §2.2). An architecture port
//! implementing the HAL therefore still acquires no transitive edge to a
//! concrete kernel crate, and the architecture-neutral kernel can name
//! the HAL without inheriting an arch dependency.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

pub mod conformance;
pub mod context;
pub mod frames;
pub mod irq;
pub mod memtag;
pub mod mmu;
pub mod percpu;
pub mod platform;
pub mod sidechannel;
pub mod timer;
pub mod tlb;
pub mod userentry;
pub mod xtlb;

pub use sidechannel::{
    conformance as sidechannel_conformance, Mitigation, MitigationEntry, MitigationProfile,
    ProfileError, SideChannelMitigation,
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

pub use mmu::{conformance as mmu_conformance, AddressSpace, MapError, PageFlags};

pub use frames::{
    conformance as frames_conformance, PageTableFrames, TableFrame, PAGE_TABLE_ENTRIES,
};

pub use tlb::{conformance as tlb_conformance, TlbShootdown};

pub use xtlb::{conformance as xtlb_conformance, CrossCpuTlbShootdown};

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
/// port during early-boot platform enumeration (`AGENTS.md` §17.2,
/// §18.2). It is deliberately distinct from *dynamic* power management
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
/// (`AGENTS.md` §1 — production code never carries a fake IPI/timer).
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
/// would constitute interface creep (`AGENTS.md` §2.4). [`Self::core_class`]
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
