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
//! [`SchedulerArch`]. The remaining HAL surface enumerated by
//! `AGENTS.md` §17.2 (context switch, MMU/page-table primitives, TLB
//! shootdown, timer programming, interrupt entry/exit, per-CPU
//! storage, and early-boot platform discovery) is migrated here as the
//! §17 burn-down advances; see `PLAN.md`. Until a primitive lives here
//! it stays in its current owning crate, and the move is tracked, not
//! silently duplicated (`AGENTS.md` §2.2).
//!
//! # Why `no_std` and dependency-free
//!
//! §17.4 permits `kernel/arch/api` to depend on `lib/*` only. The
//! crate is `no_std` and pulls in nothing so that an architecture port
//! implementing the HAL acquires no transitive edge to a concrete
//! kernel crate, and so the architecture-neutral kernel can name the
//! HAL without inheriting an arch dependency.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

/// Identifier for a logical CPU (hardware thread) the kernel manages.
///
/// Stable for the lifetime of the kernel image. Architecture ports map
/// these to APIC IDs / MPIDR / hart IDs / worker indices in their boot
/// code; the architecture-neutral kernel treats them as opaque dense
/// indices into its per-CPU arrays.
pub type CpuId = u32;

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
/// The trait is deliberately tiny. Anything more elaborate (per-core
/// timer programming, deep sleep, frequency scaling) belongs in the
/// arch crate itself, not in this surface. Growing the trait would
/// constitute interface creep (`AGENTS.md` §2.4).
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
}
