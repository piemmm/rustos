//! Host tests for the shared escalating retry cadence.
//!
//! These prove the arithmetic both consumers depend on — the maintenance
//! scheduler's member re-add and the member agent's registry re-offer — so
//! neither has to re-prove it, and neither can drift from it.

use super::{RetryCadence, RetryState, BACKOFF_CEILING_STEPS};

use tairix_abi::blkio::BlkDeviceClass;

/// A cadence with round numbers, so an escalation reads directly off the
/// assertions rather than through a class's real budget.
const CADENCE: RetryCadence = RetryCadence::new(100, 800);

#[test]
fn a_class_cadence_starts_at_that_class_grace_window() {
    for class in [
        BlkDeviceClass::Rotational,
        BlkDeviceClass::SolidState,
        BlkDeviceClass::Removable,
        BlkDeviceClass::Virtual,
    ] {
        let cadence = RetryCadence::for_class(class);
        assert_eq!(
            cadence.base_ns(),
            class.budget().grace_ns,
            "the first attempt waits out the class's own recovery grace window"
        );
        assert_eq!(
            cadence.ceiling_ns(),
            class.budget().grace_ns * BACKOFF_CEILING_STEPS,
            "and escalates to a bounded multiple of it"
        );
    }
}

#[test]
fn a_ceiling_below_the_base_is_raised_to_it() {
    let cadence = RetryCadence::new(500, 10);
    assert_eq!(cadence.base_ns(), 500);
    assert_eq!(
        cadence.ceiling_ns(),
        500,
        "an escalated wait is never shorter than the first one"
    );
}

#[test]
fn a_fresh_record_is_unarmed_and_never_due() {
    let state = RetryState::new();
    assert!(!state.is_armed());
    assert_eq!(state.due_ns(), None);
    assert!(!state.is_due(u64::MAX));
    assert_eq!(state, RetryState::default());
}

#[test]
fn arming_schedules_the_first_attempt_one_base_delay_out() {
    let mut state = RetryState::new();
    state.arm(CADENCE, 1_000);
    assert_eq!(state.due_ns(), Some(1_100));
    assert!(!state.is_due(1_099), "not due before its deadline");
    assert!(state.is_due(1_100), "due exactly at its deadline");
}

#[test]
fn re_arming_an_armed_record_does_not_restart_its_escalation() {
    let mut state = RetryState::new();
    state.arm(CADENCE, 1_000);
    state.note_failure(CADENCE, 1_100);
    let escalated = state;
    state.arm(CADENCE, 5_000);
    assert_eq!(
        state, escalated,
        "re-observing the same unfinished condition leaves the escalation alone"
    );
}

#[test]
fn each_refusal_doubles_the_delay_up_to_the_ceiling() {
    let mut state = RetryState::new();
    state.arm(CADENCE, 0);
    // 100 -> 200 -> 400 -> 800, then held at the 800 ceiling.
    for (at, expected_step) in [(100, 200), (300, 400), (700, 800), (1_500, 800)] {
        state.note_failure(CADENCE, at);
        assert_eq!(
            state.due_ns(),
            Some(at + expected_step),
            "refused at {at} waits {expected_step}"
        );
    }
}

#[test]
fn a_refusal_arms_a_record_that_was_not_armed() {
    let mut state = RetryState::new();
    state.note_failure(CADENCE, 1_000);
    assert!(state.is_armed());
    assert_eq!(
        state.due_ns(),
        Some(1_200),
        "a first attempt that fails escalates from the base rather than retrying at once"
    );
}

#[test]
fn success_clears_the_record() {
    let mut state = RetryState::new();
    state.arm(CADENCE, 0);
    state.note_failure(CADENCE, 100);
    state.disarm();
    assert_eq!(state, RetryState::new());
}

#[test]
fn a_recovery_signal_brings_the_attempt_forward_no_sooner_than_the_base_floor() {
    let mut state = RetryState::new();
    state.arm(CADENCE, 0);
    state.note_failure(CADENCE, 0);
    state.note_failure(CADENCE, 0);
    assert_eq!(state.due_ns(), Some(400), "escalated to a 400 wait");

    state.note_signal(CADENCE, 50);
    assert_eq!(
        state.due_ns(),
        Some(100),
        "the signal pulls the attempt in to one base delay after the last attempt"
    );
}

#[test]
fn a_repeating_recovery_signal_cannot_become_an_attempt_storm() {
    let mut state = RetryState::new();
    state.note_failure(CADENCE, 1_000);
    for at in 1_000..1_050 {
        state.note_signal(CADENCE, at);
        assert!(
            state.due_ns().is_some_and(|due| due >= 1_100),
            "no signal may schedule an attempt inside one base delay of the last one"
        );
    }
}

#[test]
fn a_recovery_signal_never_pushes_an_imminent_attempt_out() {
    let mut state = RetryState::new();
    state.arm(CADENCE, 0);
    assert_eq!(state.due_ns(), Some(100));
    state.note_signal(CADENCE, 10_000);
    assert_eq!(
        state.due_ns(),
        Some(100),
        "the signal only ever brings an attempt forward"
    );
}

#[test]
fn a_recovery_signal_on_an_unarmed_record_is_ignored() {
    let mut state = RetryState::new();
    state.note_signal(CADENCE, 1_000);
    assert_eq!(state, RetryState::new());
}

#[test]
fn the_arithmetic_saturates_rather_than_wrapping_near_the_clock_limit() {
    let huge = RetryCadence::new(u64::MAX / 2, u64::MAX);
    let mut state = RetryState::new();
    state.arm(huge, u64::MAX - 1);
    assert_eq!(
        state.due_ns(),
        Some(u64::MAX),
        "an attempt past the end of the clock is never scheduled in the past"
    );
    state.note_failure(huge, u64::MAX - 1);
    assert_eq!(state.due_ns(), Some(u64::MAX));
}
