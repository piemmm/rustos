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
    assert!(ipi_callback().is_none());
}

#[test]
fn per_hart_slots_track_the_registered_storage() {
    // A caller-sized backing covers exactly its `N` slots (the
    // capacity is the discovered hart count, not a baked-in `MAX_HARTS`);
    // a second backing proves registration is set-once. Declared first so
    // they precede the statements that drive them.
    static STORAGE: PreemptStorage<4> = PreemptStorage::new();
    static STORAGE2: PreemptStorage<2> = PreemptStorage::new();

    reset_preempt_storage_for_tests();

    // Before any storage is registered the per-hart observers fail closed
    // (`0` / `u32::MAX`) instead of dereferencing a null base.
    assert_eq!(per_cpu_index(0), None);
    assert_eq!(timer_interval_ticks(), 0);
    assert_eq!(timer_cpu_id(), u32::MAX);

    assert_eq!(STORAGE.register(), Ok(4));
    assert_eq!(per_cpu_index(0), Some(0));
    assert_eq!(per_cpu_index(3), Some(3));
    // An out-of-range id clamps to the last slot rather than indexing past
    // the slice end.
    assert_eq!(per_cpu_index(4), Some(3));
    assert_eq!(per_cpu_index(u32::MAX), Some(3));

    // Recording the calling hart's (host hart 0) interval/id round-trips
    // through the published slices (the bare-metal `init_local_preempt`
    // writes the same slots).
    let idx = per_cpu_index(current_hartid()).expect("registered slot");
    interval_slot(idx).store(99, Ordering::Relaxed);
    cpu_id_slot(idx).store(u64::from(0u32), Ordering::Relaxed);
    assert_eq!(timer_interval_ticks(), 99);
    assert_eq!(timer_cpu_id(), 0);

    // The tickless one-shot combiner's per-hart deadline bookkeeping
    // (Design D P-2): recording a quantum and/or a wakeup round-trips
    // through `recorded_deadlines`, and clearing with `None` removes it.
    // (The SBI-timer arming inside `reprogram` is cfg-gated to the
    // freestanding target, so on the host only the bookkeeping runs.)
    assert_eq!(recorded_deadlines(), (None, None));
    record_quantum_deadline(Some(5_000));
    assert_eq!(recorded_deadlines(), (Some(5_000), None));
    record_wakeup_deadline(Some(3_000));
    assert_eq!(recorded_deadlines(), (Some(5_000), Some(3_000)));
    record_quantum_deadline(None);
    record_wakeup_deadline(None);
    assert_eq!(recorded_deadlines(), (None, None));

    // Registration is set-once: a second backing is refused rather than
    // silently re-pointing the live slices.
    assert_eq!(
        STORAGE2.register(),
        Err(PreemptStorageError::AlreadyRegistered)
    );

    clear_for_tests();
    assert_eq!(timer_interval_ticks(), 0);
    assert_eq!(timer_cpu_id(), u32::MAX);
    reset_preempt_storage_for_tests();
}

#[test]
fn software_interrupt_enable_bit_and_cause_match_privileged_spec() {
    assert_eq!(SIE_SSIE, 0x2);
    assert_eq!(SIP_SSIP, 0x2);
    assert_eq!(SCAUSE_SUPERVISOR_SOFTWARE, 1);
}

#[test]
fn supervisor_software_interrupt_is_recognised() {
    assert!(is_supervisor_software_interrupt(
        crate::trap::SCAUSE_INTERRUPT_BIT | SCAUSE_SUPERVISOR_SOFTWARE
    ));
    // Cause 1 without the interrupt bit is the "supervisor software"
    // *exception* slot, not the IPI.
    assert!(!is_supervisor_software_interrupt(
        SCAUSE_SUPERVISOR_SOFTWARE
    ));
    // A timer interrupt is not a software interrupt.
    assert!(!is_supervisor_software_interrupt(
        crate::trap::SCAUSE_INTERRUPT_BIT | SCAUSE_SUPERVISOR_TIMER
    ));
}

#[test]
fn ipi_callback_round_trips() {
    clear_for_tests();
    assert!(ipi_callback().is_none());
    set_ipi_callback(host_cb);
    assert_eq!(
        ipi_callback().map(|f| f as *const () as usize),
        Some(host_cb as *const () as usize)
    );
    clear_for_tests();
}

#[test]
fn preempt_callback_round_trips_through_its_own_slot() {
    clear_for_tests();
    assert!(preempt_callback().is_none());
    set_preempt_callback(host_cb);
    assert_eq!(
        preempt_callback().map(|f| f as *const () as usize),
        Some(host_cb as *const () as usize)
    );
    // The preempt slot is independent of the timer and IPI slots, so
    // arming U-mode preemption never disturbs the tick/IPI dispatch.
    assert!(timer_callback().is_none());
    assert!(ipi_callback().is_none());
    clear_for_tests();
}
