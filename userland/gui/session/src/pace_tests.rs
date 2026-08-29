//! Host tests for the session's frame deadline.
//!
//! The pacer is a pure function of a monotonic instant and the compositor's
//! own damage answer, so none of this needs a kernel or a screen: how many
//! composites a flood of samples costs, that an idle desktop arms nothing,
//! and that a held frame always arms exactly one deadline and never a
//! zero-length one.

use tairix_theme::Timeline;

use super::FramePacer;
use crate::switchuser::NO_DEADLINE_NS;

/// Nanoseconds between two pointer samples from a hand on the mouse: fast
/// enough that several land inside one frame period.
const SAMPLE_NS: u64 = Timeline::FRAME_NS / 8;

#[test]
fn the_first_damaged_frame_is_admitted_at_once() {
    let mut pacer = FramePacer::new();
    assert!(
        pacer.admit(0, true),
        "a desktop coming up draws its first frame immediately"
    );
    assert_eq!(
        pacer.park_deadline_ns(0, NO_DEADLINE_NS),
        NO_DEADLINE_NS,
        "a frame that was admitted holds nothing, so it arms nothing"
    );
}

#[test]
fn a_flood_of_samples_inside_one_period_composites_once() {
    let mut pacer = FramePacer::new();
    let mut composites = 0u32;
    // Sixteen samples, all of them landing inside a single frame period.
    for step in 0..16u64 {
        if pacer.admit(step * (Timeline::FRAME_NS / 16), true) {
            composites += 1;
        }
    }
    assert_eq!(
        composites, 1,
        "the flood costs the one frame it opened with, not sixteen"
    );
}

#[test]
fn a_sustained_flood_costs_no_more_than_one_composite_per_period() {
    let mut pacer = FramePacer::new();
    let periods = 10u64;
    let span = Timeline::FRAME_NS * periods;
    let mut composites = 0u64;
    let mut at = 0u64;
    while at <= span {
        if pacer.admit(at, true) {
            composites += 1;
        }
        at += SAMPLE_NS;
    }
    assert!(
        composites <= periods + 1,
        "eight samples a period cost {composites} composites over {periods} periods"
    );
    // A producer can only composite on one of its own samples, so a
    // sample-driven flood realises a whole number of samples per frame and
    // lands just under the cadence rather than above it.
    assert!(
        composites >= periods - 1,
        "the screen still gets its frames: {composites}"
    );
}

#[test]
fn a_held_frame_arms_exactly_the_time_left_of_its_period() {
    let mut pacer = FramePacer::new();
    assert!(pacer.admit(0, true));
    assert!(
        !pacer.admit(SAMPLE_NS, true),
        "the screen cannot show it yet"
    );
    assert_eq!(
        pacer.park_deadline_ns(SAMPLE_NS, NO_DEADLINE_NS),
        Timeline::FRAME_NS - SAMPLE_NS,
        "an indefinite park is bounded to the moment the held frame comes due"
    );
    // Every later sample inside the period leaves the same one deadline,
    // counting down: the flood never re-arms and never shortens to nothing.
    for step in 2..8 {
        let now = SAMPLE_NS * step;
        assert!(!pacer.admit(now, true));
        let park = pacer.park_deadline_ns(now, NO_DEADLINE_NS);
        assert_eq!(park, Timeline::FRAME_NS - now);
        assert!(park > 0, "a deadline of nothing would be a busy poll");
    }
}

#[test]
fn a_park_already_shorter_than_the_deadline_is_left_alone() {
    let mut pacer = FramePacer::new();
    assert!(pacer.admit(0, true));
    assert!(!pacer.admit(SAMPLE_NS, true));
    assert_eq!(
        pacer.park_deadline_ns(SAMPLE_NS, SAMPLE_NS),
        SAMPLE_NS,
        "the soonest deadline wins, exactly as every other folded one does"
    );
}

#[test]
fn admitting_the_held_frame_folds_the_park_back_to_indefinite() {
    let mut pacer = FramePacer::new();
    assert!(pacer.admit(0, true));
    assert!(!pacer.admit(SAMPLE_NS, true));
    assert!(
        pacer.admit(Timeline::FRAME_NS, true),
        "the deadline elapsed, so the accumulated damage is composited"
    );
    assert_eq!(
        pacer.park_deadline_ns(Timeline::FRAME_NS, NO_DEADLINE_NS),
        NO_DEADLINE_NS
    );
}

#[test]
fn an_idle_session_arms_no_timer() {
    let mut pacer = FramePacer::new();
    assert_eq!(
        pacer.park_deadline_ns(0, NO_DEADLINE_NS),
        NO_DEADLINE_NS,
        "a session that has composited nothing owes nothing"
    );
    assert!(pacer.admit(0, true));
    // Woken repeatedly with nothing to draw — a served call, a reaped child,
    // a pressure band — the pacer arms nothing at any of them.
    for step in 1..4 {
        let now = SAMPLE_NS * step;
        assert!(pacer.admit(now, false), "an undamaged frame is never held");
        assert_eq!(pacer.park_deadline_ns(now, NO_DEADLINE_NS), NO_DEADLINE_NS);
    }
}

#[test]
fn an_undamaged_frame_does_not_start_the_period() {
    let mut pacer = FramePacer::new();
    // The frame-report deadline wakes an otherwise idle desktop to present
    // its undamaged frame. The motion that follows a moment later must not
    // be held behind a frame that moved no pixels.
    assert!(pacer.admit(0, false));
    assert!(
        pacer.admit(SAMPLE_NS, true),
        "the first frame that changed something is still the first frame"
    );
}

#[test]
fn a_frame_slower_than_the_period_is_never_held() {
    let mut pacer = FramePacer::new();
    let mut at = 0u64;
    for _ in 0..4 {
        assert!(
            pacer.admit(at, true),
            "an interaction slower than the screen pays no latency at all"
        );
        at += Timeline::FRAME_NS;
    }
}

#[test]
fn an_animations_cadence_frames_are_never_deferred() {
    // Every animated surface asks for its next frame at the shared cadence,
    // so the pacer admits each step as it is drawn: deferring one would halve
    // the frame rate of every fade on the desktop.
    let mut pacer = FramePacer::new();
    let timeline = Timeline::start(0, 200);
    let mut at = 0u64;
    let mut frames = 0u32;
    while timeline
        .next_frame_in(at)
        .is_some_and(|delta| delta == Timeline::FRAME_NS)
    {
        assert!(pacer.admit(at, true), "at {at}");
        frames += 1;
        at += Timeline::FRAME_NS;
    }
    assert!(
        frames > 10,
        "a fade of a fifth of a second is {frames} frames"
    );
}

#[test]
fn a_clock_that_jumped_backwards_admits_rather_than_stalling() {
    let mut pacer = FramePacer::new();
    assert!(pacer.admit(Timeline::FRAME_NS * 4, true));
    assert!(
        pacer.admit(0, true),
        "a jump behind the last frame must not freeze the screen for its length"
    );
    assert_eq!(pacer.park_deadline_ns(0, NO_DEADLINE_NS), NO_DEADLINE_NS);
}

#[test]
fn a_long_hold_across_a_background_spell_comes_due_rather_than_waiting() {
    // A background session folds no deadline at all, so a frame held as it
    // stepped aside is armed by nothing. It must be due the moment the
    // session is woken again rather than waiting for a deadline nobody armed.
    let mut pacer = FramePacer::new();
    assert!(pacer.admit(0, true));
    assert!(!pacer.admit(SAMPLE_NS, true));
    assert!(pacer.admit(Timeline::FRAME_NS * 10_000, true));
}
