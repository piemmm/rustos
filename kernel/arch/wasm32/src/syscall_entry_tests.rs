//! Host unit tests for the wasm32 syscall entry path.
//!
//! The argument packing, the callback storage, and [`dispatch_syscall`]
//! build and run on the host. The stateful tests share the global
//! dispatch slot, so they hold a single [`STATE_LOCK`] for their
//! duration to stay deterministic.

use super::*;
use core::sync::atomic::AtomicU64;
use std::sync::Mutex;

/// Serialises tests that mutate the shared dispatch-callback slot.
static STATE_LOCK: Mutex<()> = Mutex::new(());

/// Records the last `(number, args)` the callback saw so a test can
/// assert the packing reached the dispatcher intact.
static LAST_NUMBER: AtomicU64 = AtomicU64::new(0);
static LAST_ARG0: AtomicU64 = AtomicU64::new(0);
static LAST_ARG5: AtomicU64 = AtomicU64::new(0);

extern "C" fn recording_dispatch(number: u64, args_ptr: *const [u64; SYSCALL_MAX_ARGS]) -> u64 {
    // SAFETY: `dispatch_syscall` passes a pointer to a live, fully
    // initialised `[u64; SYSCALL_MAX_ARGS]` on its stack; the callback
    // reads it before returning, so the referent outlives this access.
    let args = unsafe { &*args_ptr };
    LAST_NUMBER.store(number, Ordering::Release);
    LAST_ARG0.store(args[0], Ordering::Release);
    LAST_ARG5.store(args[SYSCALL_MAX_ARGS - 1], Ordering::Release);
    // Return a recognisable function of the input so the caller can
    // confirm it received this callback's result.
    number.wrapping_add(args[0])
}

#[test]
fn pack_raw_args_preserves_order() {
    let args = pack_raw_args(10, 20, 30, 40, 50, 60);
    assert_eq!(args, [10, 20, 30, 40, 50, 60]);
}

#[test]
fn callback_round_trips() {
    let _guard = STATE_LOCK.lock().expect("lock");
    clear_dispatch_for_tests();
    assert!(dispatch_callback().is_none());
    set_dispatch_callback(recording_dispatch);
    assert_eq!(
        dispatch_callback().map(|f| f as *const () as usize),
        Some(recording_dispatch as *const () as usize)
    );
    clear_dispatch_for_tests();
}

#[test]
fn dispatch_without_callback_fails_closed() {
    let _guard = STATE_LOCK.lock().expect("lock");
    clear_dispatch_for_tests();
    assert_eq!(dispatch_syscall(7, [1, 2, 3, 4, 5, 6]), None);
}

#[test]
fn dispatch_marshals_number_and_args_to_the_callback() {
    let _guard = STATE_LOCK.lock().expect("lock");
    clear_dispatch_for_tests();
    set_dispatch_callback(recording_dispatch);

    let result = dispatch_syscall(3, [11, 0, 0, 0, 0, 99]);
    assert_eq!(result, Some(14)); // 3 + 11
    assert_eq!(LAST_NUMBER.load(Ordering::Acquire), 3);
    assert_eq!(LAST_ARG0.load(Ordering::Acquire), 11);
    assert_eq!(LAST_ARG5.load(Ordering::Acquire), 99);
    clear_dispatch_for_tests();
}
