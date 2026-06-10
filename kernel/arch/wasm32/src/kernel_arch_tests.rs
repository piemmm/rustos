//! Host unit tests for [`WasmArch`] and the [`ms_to_ns`] clock
//! conversion. These run under `cargo test` on the host target; the
//! `SchedulerArch` wiring and the worker map carry no wasm-specific
//! state, so the host build exercises the whole contract.

use super::*;

#[test]
fn ms_to_ns_converts_whole_milliseconds() {
    assert_eq!(ms_to_ns(0.0), 0);
    assert_eq!(ms_to_ns(1.0), 1_000_000);
    assert_eq!(ms_to_ns(1_500.0), 1_500_000_000);
}

#[test]
fn ms_to_ns_truncates_toward_zero() {
    // 1.5 µs → 1500 ns; the fractional-nanosecond part is dropped.
    assert_eq!(ms_to_ns(0.0015), 1_500);
    // A sub-nanosecond reading floors to zero rather than rounding up.
    assert_eq!(ms_to_ns(0.000_000_4), 0);
}

#[test]
fn ms_to_ns_clamps_nonpositive_and_nonfinite() {
    assert_eq!(ms_to_ns(-1.0), 0);
    assert_eq!(ms_to_ns(f64::NAN), 0);
    assert_eq!(ms_to_ns(f64::NEG_INFINITY), 0);
}

#[test]
fn ms_to_ns_saturates_instead_of_wrapping() {
    // A *finite* reading large enough to overflow `u64` nanoseconds
    // saturates rather than wrapping. (A non-finite reading clamps to
    // zero — see `ms_to_ns_clamps_nonpositive_and_nonfinite`.)
    assert_eq!(ms_to_ns(1.0e30), u64::MAX);
    assert_eq!(ms_to_ns(f64::MAX), u64::MAX);
}

#[test]
fn single_worker_handle_maps_boot_cpu_to_itself() {
    let arch = WasmArch::new(0);
    assert_eq!(arch.worker_of(0), Some(0));
    assert_eq!(arch.cpu_for_worker(0), Some(0));
    assert_eq!(arch.current_cpu(), 0);
}

#[test]
fn multi_worker_map_round_trips_cpu_and_worker_indices() {
    // Logical CPU 0 → worker 0, CPU 1 → worker 3 (a sparse host index).
    let arch = WasmArch::with_workers(0, &[0, 3]);
    assert_eq!(arch.worker_of(0), Some(0));
    assert_eq!(arch.worker_of(1), Some(3));
    assert_eq!(arch.cpu_for_worker(3), Some(1));
    assert_eq!(arch.worker_of(2), None);
    assert_eq!(arch.cpu_for_worker(7), None);
}

#[test]
fn bookkeeping_scales_to_the_discovered_worker_count() {
    // §24.1: a machine with more worker contexts than the legacy fixed
    // ceiling (`MAX_WORKERS`) is sized to its discovered count, never
    // truncated. A dense map of `MAX_WORKERS + 2` distinct workers keeps
    // every slot addressable.
    let discovered = MAX_WORKERS + 2;
    let workers: Vec<CpuId> = (0..discovered)
        .map(|w| u32::try_from(w).expect("fits u32"))
        .collect();
    let arch = WasmArch::with_workers(0, &workers);

    assert_eq!(arch.worker_capacity(), discovered);
    // The slot at the old ceiling — which the fixed-array port dropped —
    // is now populated.
    let at_old_ceiling = u32::try_from(MAX_WORKERS).expect("fits u32");
    assert_eq!(arch.worker_of(at_old_ceiling), Some(at_old_ceiling));
    // One past the discovered count remains unmapped (fail closed).
    assert_eq!(
        arch.worker_of(u32::try_from(discovered).expect("fits u32")),
        None
    );
}

#[test]
fn single_worker_handle_sizes_to_the_boot_slot() {
    // §24.1 floor: a single-worker handle reserves exactly the boot
    // CPU's own slot, no speculative headroom.
    let arch = WasmArch::new(0);
    assert_eq!(arch.worker_capacity(), 1);
    assert_eq!(arch.worker_of(1), None);
}

#[test]
fn ticks_now_is_monotonic_nondecreasing() {
    let arch = WasmArch::new(0);
    let first = arch.ticks_now();
    let second = arch.ticks_now();
    assert!(second >= first, "{second} < {first}");
}

#[test]
fn send_ipi_to_mapped_target_is_counted() {
    let arch = WasmArch::with_workers(0, &[0, 1]);
    arch.send_ipi(1);
    arch.send_ipi(1);
    assert_eq!(arch.host_ipi_count(1), 2);
    assert_eq!(arch.host_stray_ipi_count(), 0);
}

#[test]
fn send_ipi_to_self_is_permitted_and_counted() {
    let arch = WasmArch::new(0);
    arch.send_ipi(0);
    assert_eq!(arch.host_ipi_count(0), 1);
}

#[test]
fn send_ipi_to_unmapped_target_is_dropped_as_stray() {
    let arch = WasmArch::with_workers(0, &[0]);
    arch.send_ipi(5);
    assert_eq!(arch.host_stray_ipi_count(), 1);
    assert_eq!(arch.host_ipi_count(5), 0);
}

/// §17.2 / W0: the port passes the shared Arch HAL conformance vertical
/// over its real `SchedulerArch`, `SideChannel`, `MemoryTags`,
/// discovery, and per-CPU storage handles (`plans/WIRING.md` Stage W0 /
/// W2).
#[test]
fn passes_arch_hal_conformance_suite() {
    let arch = WasmArch::new(0);
    let discovery = crate::platform::HostCapabilityDiscovery::new(
        crate::platform::HostCapabilities::new(4, true),
    );
    rustos_arch_api::conformance::run_all(
        &arch,
        &crate::sidechannel::SideChannel::new(),
        &crate::memtag::MemoryTags::new(),
        &discovery,
        &crate::percpu_hal::PerCpuStorage::new(),
    );
}

/// §17.2 / W14: the port passes the secondary-bring-up conformance
/// vertical over its real `WasmArch` handle. Spawning a Web Worker is a
/// host action with no observable host-test effect, so the vertical
/// asserts the observable half — starting an unstartable id fails closed
/// and never panics. The real Worker spawn is proven by the wasm32
/// browser vertical.
#[test]
fn passes_secondary_bringup_conformance() {
    let arch = WasmArch::with_workers(0, &[0, 1]);
    rustos_arch_api::smp::conformance::run_all(&arch, CpuId::MAX);
    let erased: &dyn SecondaryBringup = &arch;
    rustos_arch_api::smp::conformance::run_all(erased, CpuId::MAX);
}

/// The boot context and any unmapped dense id are refused before asking
/// the host to spawn a worker — the fail-closed contract (`AGENTS.md`
/// §5.4.5). (The host `start_worker` substitute increments a
/// process-global counter shared with `crate::smp`'s own tests, so the
/// accepted path is exercised there, not re-driven here — no flaky
/// cross-test state, `AGENTS.md` §7.)
#[test]
fn start_secondary_rejects_boot_and_unmapped_ids() {
    let arch = WasmArch::with_workers(0, &[0, 1]);
    // SAFETY: both ids below are refused before any host spawn, so this
    // test takes no platform action and touches no shared global state.
    unsafe {
        assert_eq!(arch.start_secondary(0), Err(SmpError::InvalidCpu));
        assert_eq!(arch.start_secondary(7), Err(SmpError::InvalidCpu));
        assert_eq!(arch.start_secondary(u32::MAX), Err(SmpError::InvalidCpu));
    }
}
