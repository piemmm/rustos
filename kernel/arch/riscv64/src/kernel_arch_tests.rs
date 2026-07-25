//! Host unit tests for [`RiscvArch`] (tests live in
//! their own file beside the code they cover).

use super::*;

// Each test owns a distinct function-local `static` backing — the same
// allocator-free `&'static`-storage pattern the bare-metal verticals
// use — so no two handles alias one another's
// per-CPU bookkeeping under the parallel test runner (no flaky shared state). Every test constructs exactly one handle, so a
// single local `static` per test suffices.

#[test]
fn current_cpu_returns_boot_cpu() {
    static S: RiscvArchStorage<1> = RiscvArchStorage::new();
    let arch = RiscvArch::new(&S, 0, 10_000_000);
    assert_eq!(arch.current_cpu(), 0);
}

#[test]
fn ticks_now_is_monotonic_on_host() {
    static S: RiscvArchStorage<1> = RiscvArchStorage::new();
    let arch = RiscvArch::new(&S, 0, 10_000_000);
    let a = arch.ticks_now();
    let b = arch.ticks_now();
    assert!(b > a);
}

#[test]
fn monotonic_ns_is_non_decreasing_on_host() {
    // The host clock counts ticks 1, 2, 3, …; at 1 GHz a tick is one
    // nanosecond, so the readings are strictly increasing.
    static S: RiscvArchStorage<1> = RiscvArchStorage::new();
    let arch = RiscvArch::new(&S, 0, 1_000_000_000);
    let a = arch.monotonic_ns();
    let b = arch.monotonic_ns();
    let c = arch.monotonic_ns();
    assert!(b >= a, "expected b >= a, got a={a} b={b}");
    assert!(c >= b, "expected c >= b, got b={b} c={c}");
}

#[test]
fn timebase_is_round_tripped() {
    static S: RiscvArchStorage<4> = RiscvArchStorage::new();
    let arch = RiscvArch::new(&S, 3, 24_000_000);
    assert_eq!(arch.timebase_hz(), 24_000_000);
}

#[test]
fn zero_timebase_does_not_divide_by_zero() {
    // A malformed (zero) frequency must not trap; `monotonic_ns` clamps
    // the divisor to 1 (fail safe).
    static S: RiscvArchStorage<1> = RiscvArchStorage::new();
    let arch = RiscvArch::new(&S, 0, 0);
    let _ = arch.monotonic_ns();
}

#[test]
fn single_hart_new_maps_boot_cpu_to_its_own_hartid() {
    static S: RiscvArchStorage<2> = RiscvArchStorage::new();
    let arch = RiscvArch::new(&S, 0, 10_000_000);
    assert_eq!(arch.hartid_of(0), Some(0));
    assert_eq!(arch.hartid_of(1), None);
    assert_eq!(arch.cpu_for_hartid(0), Some(0));
    assert_eq!(arch.cpu_for_hartid(7), None);
}

#[test]
fn with_harts_builds_a_dense_cpu_to_hartid_map() {
    // Logical CPU 0 → hart 0, CPU 1 → hart 1 (the multi-hart vertical's
    // identity layout), plus a non-identity third entry.
    static S: RiscvArchStorage<3> = RiscvArchStorage::new();
    let arch = RiscvArch::with_harts(&S, 0, 10_000_000, &[0, 1, 5]);
    assert_eq!(arch.hartid_of(0), Some(0));
    assert_eq!(arch.hartid_of(1), Some(1));
    assert_eq!(arch.hartid_of(2), Some(5));
    assert_eq!(arch.cpu_for_hartid(5), Some(2));
    assert_eq!(arch.cpu_for_hartid(1), Some(1));
    assert_eq!(arch.cpu_for_hartid(3), None);
}

#[test]
fn send_ipi_counts_mapped_targets_on_host() {
    static S: RiscvArchStorage<2> = RiscvArchStorage::new();
    let arch = RiscvArch::with_harts(&S, 0, 10_000_000, &[0, 1]);
    arch.send_ipi(1);
    arch.send_ipi(1);
    arch.send_ipi(0);
    assert_eq!(arch.host_ipi_count(1), 2);
    assert_eq!(arch.host_ipi_count(0), 1);
    assert_eq!(arch.host_stray_ipi_count(), 0);
}

#[test]
fn send_ipi_drops_unmapped_target_into_stray_counter() {
    static S: RiscvArchStorage<1> = RiscvArchStorage::new();
    let arch = RiscvArch::new(&S, 0, 10_000_000);
    // CPU 4 is unmapped in the single-hart map.
    arch.send_ipi(4);
    // u32::MAX is out of range of the hart pool.
    arch.send_ipi(u32::MAX);
    assert_eq!(arch.host_stray_ipi_count(), 2);
    assert_eq!(arch.host_ipi_count(4), 0);
}

/// / W0: the port passes the shared Arch HAL conformance vertical
/// over its real `SchedulerArch`, `SideChannel`, `MemoryTags`,
/// discovery, and per-CPU storage handles (`plans/WIRING.md` Stage W0 /
/// W2).
#[test]
fn passes_arch_hal_conformance_suite() {
    static S: RiscvArchStorage<1> = RiscvArchStorage::new();
    let arch = RiscvArch::new(&S, 0, 10_000_000);
    let blob = crate::fdt::tests::virt_like(0x8000_0000, 0x1000_0000, 10_000_000);
    let fdt = crate::fdt::Fdt::new(&blob).expect("valid fdt");
    let discovery = crate::platform::FdtDiscovery::new(fdt);
    tairix_arch_api::conformance::run_all(
        &arch,
        &crate::sidechannel::SideChannel::new(),
        &crate::memtag::MemoryTags::new(),
        &discovery,
        &crate::percpu_hal::PerCpuStorage::new(),
        &crate::cpufeatures::CpuFeatureDetect::new(),
    );
    // The cycle-counter slice has its own vertical.
    tairix_arch_api::cpucycles::conformance::run_all(&crate::cpufeatures::CpuCycleCounter::new());
}

/// / W6: the port passes the cross-CPU TLB-shootdown conformance
/// vertical over its real `RiscvArch` handle. On the host the local
/// `sfence.vma` is a vacuous no-op (no TLB) and there is no firmware to
/// call, so the vertical asserts the observable half — the call is total
/// and panic-free for any address. The real local-`sfence` + SBI
/// `remote_sfence_vma` round-trip is proven by
/// `cross_cpu_tlb_shootdown_qemu_riscv64`.
#[test]
fn passes_cross_cpu_tlb_shootdown_conformance() {
    static S: RiscvArchStorage<2> = RiscvArchStorage::new();
    let arch = RiscvArch::with_harts(&S, 0, 10_000_000, &[0, 1]);
    tairix_arch_api::xtlb::conformance::run_all(&arch, 100u64 << 30);
    let erased: &dyn CrossCpuTlbShootdown = &arch;
    tairix_arch_api::xtlb::conformance::run_all(erased, 100u64 << 30);
}

/// / W14: the port passes the secondary-bring-up conformance
/// vertical over its real `RiscvArch` handle. On the host there is no SBI
/// firmware, so the vertical asserts the observable half — starting an
/// unstartable id fails closed and never panics. The real SBI HSM
/// `hart_start` round-trip is proven by the multi-hart QEMU verticals
/// (`ipi_smp_qemu_riscv64`, `cross_cpu_tlb_shootdown_qemu_riscv64`).
#[test]
fn passes_secondary_bringup_conformance() {
    static S: RiscvArchStorage<2> = RiscvArchStorage::new();
    let arch = RiscvArch::with_harts(&S, 0, 10_000_000, &[0, 1]);
    tairix_arch_api::smp::conformance::run_all(&arch, CpuId::MAX);
    let erased: &dyn SecondaryBringup = &arch;
    tairix_arch_api::smp::conformance::run_all(erased, CpuId::MAX);
}

/// The boot hart and any unmapped / out-of-range dense id are refused
/// before any firmware call — the fail-closed contract. (The set-once secondary-entry slot is a process-global
/// shared with `crate::smp`'s own tests, so it is exercised there, not
/// re-driven here, no flaky cross-test state.)
#[test]
fn start_secondary_rejects_boot_and_unmapped_ids() {
    static S: RiscvArchStorage<2> = RiscvArchStorage::new();
    let arch = RiscvArch::with_harts(&S, 0, 10_000_000, &[0, 1]);
    // SAFETY: the host build of `start_secondary` issues no SBI call; it
    // only validates the id. Both ids below are refused before the
    // entry-slot check, so this test touches no shared global state.
    unsafe {
        // Boot hart: already running.
        assert_eq!(arch.start_secondary(0), Err(SmpError::InvalidCpu));
        // Unmapped dense id.
        assert_eq!(arch.start_secondary(5), Err(SmpError::InvalidCpu));
        // Out-of-range id (beyond the hart pool).
        assert_eq!(arch.start_secondary(u32::MAX), Err(SmpError::InvalidCpu));
    }
}

/// Compile-time proof that `RiscvArch` implements [`SchedulerArch`].
const _IS_SCHED_ARCH: fn(&RiscvArch) -> CpuId = <RiscvArch as SchedulerArch>::current_cpu;
