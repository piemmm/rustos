//! The Arch HAL conformance vertical.
//!
//! Parity between architecture ports is *enforced*, never asserted by
//! inspection (`plans/WIRING.md` §0.3): a port is "at x86_64 level" for
//! a HAL slice only when it passes that slice's conformance suite. This
//! module is the harness those per-port suites run, modelled on the
//! `kernel/sched/api` [`conformance`](crate) suite every concrete
//! scheduler must pass.
//!
//! # What it checks
//!
//! * [`run_scheduler_arch`] — the [`SchedulerArch`] contract
//!   (`current_cpu` stable, `ticks_now` monotonically non-decreasing,
//!   `send_ipi` to self a no-op-equivalent, `core_class` total and
//!   panic-free for every input including an out-of-range [`CpuId`]).
//! * [`run_all`] — the whole HAL slice migrated so far: it runs
//!   [`run_scheduler_arch`] **and** the side-channel vertical
//!   ([`sidechannel::conformance::run_all`]),
//!   the memory-tagging vertical
//!   ([`memtag::conformance::run_all`]),
//!   the platform-discovery vertical
//!   ([`platform::conformance::run`]),
//!   and the per-CPU storage round-trip vertical
//!   ([`percpu::conformance::run_all`])
//!   over the same port's handles.
//!
//! # Why one handle per trait
//!
//! Each port implements the HAL traits on distinct types (the
//! `*Arch` scheduler handle, the `SideChannel` handle, the `MemoryTags`
//! handle, the discovery handle, the per-CPU storage handle), so
//! [`run_all`] takes one reference per trait rather than assuming a
//! single god-object. The suite names only the traits — never
//! a concrete port — so the same source is a valid acceptance test for
//! every present and future architecture. It
//! is host-run and deterministic, exactly like the scheduler-policy
//! suite (no flaky tests).
//!
//! The per-CPU *isolation* property (one CPU's word is independent of
//! another's) needs two handles, which a single-handle [`run_all`] cannot
//! express; each port drives [`percpu::conformance::run_isolation`] over
//! two handles in its own suite.

use crate::{
    memtag, percpu, platform, sidechannel, CpuId, MemoryTagging, PerCpu, PlatformDiscovery,
    SchedulerArch, SideChannelMitigation,
};

/// Run the [`SchedulerArch`] contract suite against `arch`.
///
/// This is the scheduler-facing slice of the HAL conformance vertical.
/// It exercises only the public [`SchedulerArch`] surface, so it drives
/// any port (or the host `TestArch`) without naming a concrete type.
///
/// # Panics
///
/// Panics (failing the test) if any required property does not hold:
/// `current_cpu` is unstable across back-to-back calls, `ticks_now` goes
/// backwards, a `send_ipi` or `core_class` call panics, or `core_class`
/// disagrees with itself for the same [`CpuId`].
pub fn run_scheduler_arch<A: SchedulerArch + ?Sized>(arch: &A) {
    current_cpu_is_stable(arch);
    ticks_are_monotonic(arch);
    send_ipi_to_self_is_a_noop(arch);
    core_class_is_total(arch);
}

/// Run the entire migrated Arch HAL conformance vertical against a port.
///
/// Combines the [`SchedulerArch`] contract ([`run_scheduler_arch`]) with
/// the side-channel vertical, the memory-tagging vertical,
/// the early-boot platform-discovery vertical, and the per-CPU
/// storage round-trip vertical already defined in this crate, each over
/// the matching port handle.
///
/// # Panics
///
/// Panics (failing the test) if any of the five slices fails its
/// contract; see [`run_scheduler_arch`],
/// [`sidechannel::conformance::run_all`],
/// [`memtag::conformance::run_all`],
/// [`platform::conformance::run`], and
/// [`percpu::conformance::run_all`].
pub fn run_all<A, S, M, P, C>(
    arch: &A,
    side_channel: &S,
    memory_tagging: &M,
    platform_discovery: &P,
    per_cpu: &C,
) where
    A: SchedulerArch + ?Sized,
    S: SideChannelMitigation + ?Sized,
    M: MemoryTagging + ?Sized,
    P: PlatformDiscovery + ?Sized,
    C: PerCpu + ?Sized,
{
    run_scheduler_arch(arch);
    sidechannel::conformance::run_all(side_channel);
    memtag::conformance::run_all(memory_tagging);
    platform::conformance::run(platform_discovery);
    percpu::conformance::run_all(per_cpu);
}

/// `current_cpu` is stable for the duration of a call: repeated reads from one execution context agree.
fn current_cpu_is_stable<A: SchedulerArch + ?Sized>(arch: &A) {
    let first = arch.current_cpu();
    assert_eq!(
        arch.current_cpu(),
        first,
        "current_cpu must be stable across back-to-back calls"
    );
    assert_eq!(
        arch.current_cpu(),
        first,
        "current_cpu must be stable across back-to-back calls"
    );
}

/// `ticks_now` is monotonically non-decreasing: a
/// later read never returns a smaller value than an earlier one.
fn ticks_are_monotonic<A: SchedulerArch + ?Sized>(arch: &A) {
    let mut last = arch.ticks_now();
    for _ in 0..8 {
        let now = arch.ticks_now();
        assert!(
            now >= last,
            "ticks_now went backwards: {now} < {last} (must be monotonically non-decreasing)"
        );
        last = now;
    }
}

/// Sending an IPI to the calling CPU is permitted and is a no-op
/// equivalent to a self-reschedule: it must not
/// panic. Targeting an arbitrary (possibly unmapped) CPU is also
/// best-effort and must not panic.
fn send_ipi_to_self_is_a_noop<A: SchedulerArch + ?Sized>(arch: &A) {
    let me = arch.current_cpu();
    arch.send_ipi(me);
    arch.send_ipi(me);
    // A stray / out-of-range target is dropped best-effort, never a panic.
    arch.send_ipi(CpuId::MAX);
}

/// `core_class` is total: it returns a valid class for every input,
/// including an out-of-range [`CpuId`], and never panics. It also agrees
/// with itself — the class is a static per-CPU identity.
fn core_class_is_total<A: SchedulerArch + ?Sized>(arch: &A) {
    for cpu in [0, arch.current_cpu(), CpuId::MAX] {
        let class = arch.core_class(cpu);
        assert_eq!(
            arch.core_class(cpu),
            class,
            "core_class must be a stable static identity for cpu {cpu}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memtag::{MemoryTagging, Tagging, TaggingProfile, TAG_COUNT};
    use crate::platform::{DiscoveryError, HwNodeSink, PlatformDiscovery};
    use crate::sidechannel::{Mitigation, MitigationProfile, SideChannelMitigation};
    use crate::{CoreClass, PerCpu};
    use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use tairix_abi::{HwDeviceClass, HwNode, HW_NODE_ROOT};

    /// A faithful host stub of a [`SchedulerArch`] port: a fixed CPU id,
    /// a monotonic tick counter, and a best-effort `send_ipi`.
    #[derive(Default)]
    struct StubArch {
        cpu: CpuId,
        ticks: AtomicU64,
    }

    impl SchedulerArch for StubArch {
        fn current_cpu(&self) -> CpuId {
            self.cpu
        }

        fn ticks_now(&self) -> u64 {
            self.ticks.fetch_add(1, Ordering::Relaxed) + 1
        }

        fn send_ipi(&self, _target: CpuId) {}
    }

    struct StubSideChannel;

    impl SideChannelMitigation for StubSideChannel {
        fn profile(&self) -> MitigationProfile {
            MitigationProfile {
                address_space_isolation: Mitigation::Applied,
                syscall_entry_barrier: Mitigation::Applied,
                syscall_exit_barrier: Mitigation::Applied,
                context_switch_buffer_flush: Mitigation::Applied,
                context_switch_indirect_branch_barrier: Mitigation::Applied,
            }
        }
        fn syscall_entry_barrier(&self) {}
        fn syscall_exit_barrier(&self) {}
        fn context_switch_barrier(&self) {}
    }

    struct StubDiscovery;

    impl PlatformDiscovery for StubDiscovery {
        fn discover(&self, sink: &mut dyn HwNodeSink) -> Result<(), DiscoveryError> {
            sink.emit(HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Root))?;
            sink.emit(HwNode::new(1, 0, HwDeviceClass::Cpu))?;
            Ok(())
        }
    }

    #[derive(Default)]
    struct StubPerCpu {
        base: AtomicUsize,
    }

    impl PerCpu for StubPerCpu {
        fn read_self_base(&self) -> usize {
            self.base.load(Ordering::Relaxed)
        }
        unsafe fn write_self_base(&self, base: usize) {
            self.base.store(base, Ordering::Relaxed);
        }
    }

    struct StubMemTags;

    impl MemoryTagging for StubMemTags {
        fn profile(&self) -> TaggingProfile {
            TaggingProfile {
                tag_storage: Tagging::Supported,
                tag_check_faults: Tagging::Supported,
            }
        }
        fn granule_bytes(&self) -> usize {
            16
        }
        fn tag_count(&self) -> u8 {
            TAG_COUNT
        }
    }

    #[test]
    fn scheduler_arch_suite_accepts_an_honest_stub() {
        let arch = StubArch {
            cpu: 2,
            ..StubArch::default()
        };
        run_scheduler_arch(&arch);
        // Object-safe: the kernel reaches the handle through `&dyn`.
        let dynamic: &dyn SchedulerArch = &arch;
        run_scheduler_arch(dynamic);
    }

    #[test]
    fn run_all_accepts_an_honest_port() {
        let arch = StubArch::default();
        run_all(
            &arch,
            &StubSideChannel,
            &StubMemTags,
            &StubDiscovery,
            &StubPerCpu::default(),
        );
    }

    /// A port whose `core_class` panics on an out-of-range CPU violates
    /// the totality contract; the suite must catch it.
    struct PanickingCoreClass;

    impl SchedulerArch for PanickingCoreClass {
        fn current_cpu(&self) -> CpuId {
            0
        }
        fn ticks_now(&self) -> u64 {
            0
        }
        fn send_ipi(&self, _target: CpuId) {}
        fn core_class(&self, cpu: CpuId) -> CoreClass {
            assert_ne!(cpu, CpuId::MAX, "port forgot to bound the CPU index");
            CoreClass::Performance
        }
    }

    #[test]
    #[should_panic(expected = "port forgot to bound the CPU index")]
    fn suite_rejects_a_non_total_core_class() {
        run_scheduler_arch(&PanickingCoreClass);
    }

    /// A port whose tick source runs backwards must be rejected.
    struct BackwardsTicks;

    impl SchedulerArch for BackwardsTicks {
        fn current_cpu(&self) -> CpuId {
            0
        }
        fn ticks_now(&self) -> u64 {
            // First call high, every later call low: non-monotonic.
            static FIRST: AtomicU64 = AtomicU64::new(0);
            if FIRST.swap(1, Ordering::Relaxed) == 0 {
                100
            } else {
                1
            }
        }
        fn send_ipi(&self, _target: CpuId) {}
    }

    #[test]
    #[should_panic(expected = "ticks_now went backwards")]
    fn suite_rejects_a_backwards_tick_source() {
        run_scheduler_arch(&BackwardsTicks);
    }
}
