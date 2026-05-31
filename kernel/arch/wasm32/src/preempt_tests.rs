//! Host unit tests for the cooperative-preemption surface.
//!
//! The callback storage, the `CpuId` slot, the tick counter, and the
//! budget helper build and run on the host; the host
//! `requestAnimationFrame` re-arm is a no-op off the wasm target, so a
//! test drives [`on_animation_frame`] directly.
//!
//! The stateful tests share this module's global statics, so they hold a
//! single [`STATE_LOCK`] for their duration. Serialising them keeps the
//! suite deterministic rather than flaky (`AGENTS.md` §7).

use super::*;
use std::sync::Mutex;

/// Serialises tests that mutate the shared callback / counter statics.
static STATE_LOCK: Mutex<()> = Mutex::new(());

extern "C" fn record_a(_cpu: CpuId) {
    HITS_A.fetch_add(1, Ordering::AcqRel);
}

extern "C" fn record_b(_cpu: CpuId) {
    HITS_B.fetch_add(1, Ordering::AcqRel);
}

static HITS_A: AtomicU64 = AtomicU64::new(0);
static HITS_B: AtomicU64 = AtomicU64::new(0);

#[test]
fn budget_exhausted_at_or_past_the_limit() {
    assert!(!cooperative_budget_exhausted(0.0, 8.0));
    assert!(!cooperative_budget_exhausted(7.9, 8.0));
    assert!(cooperative_budget_exhausted(8.0, 8.0));
    assert!(cooperative_budget_exhausted(100.0, 8.0));
}

#[test]
fn budget_fails_closed_on_degenerate_inputs() {
    // A non-positive or non-finite budget yields immediately.
    assert!(cooperative_budget_exhausted(0.0, 0.0));
    assert!(cooperative_budget_exhausted(0.0, -1.0));
    assert!(cooperative_budget_exhausted(0.0, f64::NAN));
    // A non-finite elapsed reading also yields.
    assert!(cooperative_budget_exhausted(f64::INFINITY, 8.0));
}

#[test]
fn callbacks_round_trip() {
    let _guard = STATE_LOCK.lock().expect("lock");
    clear_for_tests();
    assert!(tick_callback().is_none());
    assert!(ipi_callback().is_none());

    set_tick_callback(record_a);
    set_ipi_callback(record_b);
    assert_eq!(
        tick_callback().map(|f| f as *const () as usize),
        Some(record_a as *const () as usize)
    );
    assert_eq!(
        ipi_callback().map(|f| f as *const () as usize),
        Some(record_b as *const () as usize)
    );
    clear_for_tests();
}

#[test]
fn diagnostic_slots_start_clear() {
    let _guard = STATE_LOCK.lock().expect("lock");
    clear_for_tests();
    assert_eq!(tick_cpu_id(), u32::MAX);
    assert_eq!(tick_count(), 0);
    assert!(tick_callback().is_none());
}

#[test]
fn animation_frame_drives_the_tick_callback_and_counts() {
    let _guard = STATE_LOCK.lock().expect("lock");
    clear_for_tests();
    HITS_A.store(0, Ordering::Release);

    set_tick_callback(record_a);
    init_local_preempt(2);
    assert_eq!(tick_cpu_id(), 2);

    on_animation_frame();
    on_animation_frame();
    on_animation_frame();

    assert_eq!(HITS_A.load(Ordering::Acquire), 3);
    assert_eq!(tick_count(), 3);
    clear_for_tests();
}

#[test]
fn animation_frame_without_recorded_cpu_does_not_fire() {
    let _guard = STATE_LOCK.lock().expect("lock");
    clear_for_tests();
    HITS_A.store(0, Ordering::Release);

    // Callback installed but `init_local_preempt` never called: no
    // recorded `CpuId`, so the tick is a no-op (fail closed).
    set_tick_callback(record_a);
    on_animation_frame();

    assert_eq!(HITS_A.load(Ordering::Acquire), 0);
    assert_eq!(tick_count(), 0);
    clear_for_tests();
}

#[test]
fn ipi_message_drives_the_ipi_callback() {
    let _guard = STATE_LOCK.lock().expect("lock");
    clear_for_tests();
    HITS_B.store(0, Ordering::Release);

    set_ipi_callback(record_b);
    init_local_preempt(1);
    on_ipi_message();
    on_ipi_message();

    assert_eq!(HITS_B.load(Ordering::Acquire), 2);
    // An IPI does not advance the animation-frame tick counter.
    assert_eq!(tick_count(), 0);
    clear_for_tests();
}
