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
fn with_workers_ignores_entries_beyond_capacity() {
    let many: [CpuId; MAX_WORKERS + 2] = [0; MAX_WORKERS + 2];
    let arch = WasmArch::with_workers(0, &many);
    // The last two entries are dropped; indexing them is `None`.
    assert_eq!(
        arch.worker_of(u32::try_from(MAX_WORKERS).expect("fits u32")),
        None
    );
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
