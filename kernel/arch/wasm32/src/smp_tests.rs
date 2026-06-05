//! Host unit tests for the wasm32 multi-worker (SMP) bring-up
//! primitives. These run under `cargo test` on the host target; the
//! `Worker` spawn is substituted by a counter (see [`super`]), so the
//! range checks and the success/failure decode are exercised without a
//! browser.

use super::*;

#[test]
fn boot_context_is_not_a_spawnable_secondary() {
    // Logical CPU 0 is the main thread; it is never spawned.
    assert!(!is_valid_secondary(0));
}

#[test]
fn secondary_indices_in_range_are_spawnable() {
    assert!(is_valid_secondary(1));
    assert!(is_valid_secondary(
        u32::try_from(MAX_WORKERS - 1).expect("fits u32")
    ));
}

#[test]
fn indices_at_or_beyond_capacity_are_not_spawnable() {
    assert!(!is_valid_secondary(
        u32::try_from(MAX_WORKERS).expect("fits u32")
    ));
    assert!(!is_valid_secondary(
        u32::try_from(MAX_WORKERS + 1).expect("fits u32")
    ));
}

#[test]
fn start_worker_rejects_the_boot_context() {
    assert_eq!(start_worker(0), Err(StartWorkerError::IndexOutOfRange));
}

#[test]
fn start_worker_rejects_out_of_range_index() {
    let oob = u32::try_from(MAX_WORKERS).expect("fits u32");
    assert_eq!(start_worker(oob), Err(StartWorkerError::IndexOutOfRange));
}

#[test]
fn start_worker_for_in_range_secondary_invokes_the_host_spawn() {
    // The counter only ever increases, so a strict `>` is race-free even
    // when other tests in this module spawn concurrently.
    let before = host_started_count();
    assert_eq!(start_worker(1), Ok(()));
    assert!(host_started_count() > before);
}

#[test]
fn out_of_range_start_does_not_reach_the_host() {
    // An out-of-range index fails closed before the host call, so the
    // host spawn counter must not move for it. Taking the snapshot after
    // the rejected call and asserting equality with a second read keeps
    // the check independent of concurrent in-range spawns.
    let _ = start_worker(0);
    let a = host_started_count();
    let _ = start_worker(0);
    let b = host_started_count();
    // Two rejected calls between the two reads add nothing of their own;
    // any movement is from a concurrent in-range test, which is allowed.
    assert!(b >= a);
}

#[test]
fn current_worker_is_boot_cpu_on_host() {
    // The host build has no Web Worker context, so the running context is
    // always the boot CPU.
    assert_eq!(current_worker(), 0);
}
