//! Host unit tests for [`RiscvArch`] (`AGENTS.md` §7 — tests live in
//! their own file beside the code they cover).

use super::*;

#[test]
fn current_cpu_returns_boot_cpu() {
    let arch = RiscvArch::new(0, 10_000_000);
    assert_eq!(arch.current_cpu(), 0);
}

#[test]
fn ticks_now_is_monotonic_on_host() {
    let arch = RiscvArch::new(0, 10_000_000);
    let a = arch.ticks_now();
    let b = arch.ticks_now();
    assert!(b > a);
}

#[test]
fn monotonic_ns_is_non_decreasing_on_host() {
    // The host clock counts ticks 1, 2, 3, …; at 1 GHz a tick is one
    // nanosecond, so the readings are strictly increasing.
    let arch = RiscvArch::new(0, 1_000_000_000);
    let a = arch.monotonic_ns();
    let b = arch.monotonic_ns();
    let c = arch.monotonic_ns();
    assert!(b >= a, "expected b >= a, got a={a} b={b}");
    assert!(c >= b, "expected c >= b, got b={b} c={c}");
}

#[test]
fn timebase_is_round_tripped() {
    let arch = RiscvArch::new(3, 24_000_000);
    assert_eq!(arch.timebase_hz(), 24_000_000);
}

#[test]
fn zero_timebase_does_not_divide_by_zero() {
    // A malformed (zero) frequency must not trap; `monotonic_ns` clamps
    // the divisor to 1 (`AGENTS.md` §2.9 — fail safe).
    let arch = RiscvArch::new(0, 0);
    let _ = arch.monotonic_ns();
}

#[test]
fn single_hart_new_maps_boot_cpu_to_its_own_hartid() {
    let arch = RiscvArch::new(0, 10_000_000);
    assert_eq!(arch.hartid_of(0), Some(0));
    assert_eq!(arch.hartid_of(1), None);
    assert_eq!(arch.cpu_for_hartid(0), Some(0));
    assert_eq!(arch.cpu_for_hartid(7), None);
}

#[test]
fn with_harts_builds_a_dense_cpu_to_hartid_map() {
    // Logical CPU 0 → hart 0, CPU 1 → hart 1 (the multi-hart vertical's
    // identity layout), plus a non-identity third entry.
    let arch = RiscvArch::with_harts(0, 10_000_000, &[0, 1, 5]);
    assert_eq!(arch.hartid_of(0), Some(0));
    assert_eq!(arch.hartid_of(1), Some(1));
    assert_eq!(arch.hartid_of(2), Some(5));
    assert_eq!(arch.cpu_for_hartid(5), Some(2));
    assert_eq!(arch.cpu_for_hartid(1), Some(1));
    assert_eq!(arch.cpu_for_hartid(3), None);
}

#[test]
fn send_ipi_counts_mapped_targets_on_host() {
    let arch = RiscvArch::with_harts(0, 10_000_000, &[0, 1]);
    arch.send_ipi(1);
    arch.send_ipi(1);
    arch.send_ipi(0);
    assert_eq!(arch.host_ipi_count(1), 2);
    assert_eq!(arch.host_ipi_count(0), 1);
    assert_eq!(arch.host_stray_ipi_count(), 0);
}

#[test]
fn send_ipi_drops_unmapped_target_into_stray_counter() {
    let arch = RiscvArch::new(0, 10_000_000);
    // CPU 4 is unmapped in the single-hart map.
    arch.send_ipi(4);
    // u32::MAX is out of range of the hart pool.
    arch.send_ipi(u32::MAX);
    assert_eq!(arch.host_stray_ipi_count(), 2);
    assert_eq!(arch.host_ipi_count(4), 0);
}

/// §17.2 / W0: the port passes the shared Arch HAL conformance vertical
/// over its real `SchedulerArch`, `SideChannel`, and `MemoryTags`
/// handles (`plans/WIRING.md` Stage W0).
#[test]
fn passes_arch_hal_conformance_suite() {
    let arch = RiscvArch::new(0, 10_000_000);
    rustos_arch_api::conformance::run_all(
        &arch,
        &crate::sidechannel::SideChannel::new(),
        &crate::memtag::MemoryTags::new(),
    );
}

/// Compile-time proof that `RiscvArch` implements [`SchedulerArch`].
const _IS_SCHED_ARCH: fn(&RiscvArch) -> CpuId = <RiscvArch as SchedulerArch>::current_cpu;
