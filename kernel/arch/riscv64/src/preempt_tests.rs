//! Host unit tests for the supervisor-timer preemption surface.
//!
//! The callback storage, the interval/`CpuId` slots, the `scause`
//! decode, and the interval arithmetic build and run on the host; the
//! SBI re-arm and the `sie.STIE` CSR write are exercised by the
//! timer-drives-scheduler QEMU vertical.

use super::*;

extern "C" fn host_cb(_cpu: CpuId) {}

#[test]
fn enable_bit_and_cause_match_privileged_spec() {
    assert_eq!(SIE_STIE, 0x20);
    assert_eq!(SCAUSE_SUPERVISOR_TIMER, 5);
}

#[test]
fn supervisor_timer_interrupt_is_recognised() {
    assert!(is_supervisor_timer_interrupt(
        crate::trap::SCAUSE_INTERRUPT_BIT | SCAUSE_SUPERVISOR_TIMER
    ));
}

#[test]
fn synchronous_exception_with_code_5_is_not_a_timer_interrupt() {
    // Cause 5 without the interrupt bit is a load access fault.
    assert!(!is_supervisor_timer_interrupt(SCAUSE_SUPERVISOR_TIMER));
}

#[test]
fn external_interrupt_is_not_a_timer_interrupt() {
    assert!(!is_supervisor_timer_interrupt(
        crate::trap::SCAUSE_INTERRUPT_BIT | crate::trap::SCAUSE_SUPERVISOR_EXTERNAL
    ));
}

#[test]
fn callback_round_trips() {
    clear_for_tests();
    assert!(timer_callback().is_none());
    set_timer_callback(host_cb);
    assert_eq!(
        timer_callback().map(|f| f as *const () as usize),
        Some(host_cb as *const () as usize)
    );
    clear_for_tests();
}

#[test]
fn interval_for_hz_divides_the_timebase() {
    // 10 MHz timebase (the QEMU `virt` default) at 100 Hz → 100_000.
    assert_eq!(interval_for_hz(10_000_000, 100), 100_000);
}

#[test]
fn interval_for_hz_clamps_to_at_least_one() {
    // hz greater than the timebase would divide to 0; clamp to 1 so the
    // timer always advances.
    assert_eq!(interval_for_hz(100, 1000), 1);
    // hz == 0 is treated as 1 Hz (no division by zero).
    assert_eq!(interval_for_hz(50, 0), 50);
}

#[test]
fn diagnostic_slots_start_clear() {
    clear_for_tests();
    assert_eq!(timer_interval_ticks(), 0);
    assert_eq!(timer_cpu_id(), u32::MAX);
}
