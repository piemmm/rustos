//! Host tests for the desktop backdrop's crossfade.
//!
//! The state machine is a pure function of the clock: what a frame part-way
//! through owes, when the desktop must next wake to draw one, that arriving
//! releases the screen-sized copy of the ground being left, and that a
//! reduced-motion theme is arrived before it has drawn anything.

use tairix_wm::{Color, Surface};

use super::BackdropFade;
use crate::switchuser::NO_DEADLINE_NS;

/// The span the desktop's own theme gives a backdrop change, in milliseconds.
const SPAN_MS: u16 = 600;

/// That span in nanoseconds, for readable instants.
const SPAN_NS: u64 = SPAN_MS as u64 * 1_000_000;

/// An arbitrary instant to begin at, well away from zero so a test cannot
/// pass by treating "no time yet" as "arrived".
const BEGAN_NS: u64 = 90_000_000_000;

/// A ground to leave behind: a small opaque surface standing in for the
/// screen-sized one the shell flattens.
fn ground() -> Surface {
    let mut surface = Surface::new(4, 4).expect("allocates");
    surface.fill(Color::rgb(200, 100, 50));
    surface
}

#[test]
fn a_backdrop_that_has_never_changed_owes_nothing() {
    let fade = BackdropFade::default();
    assert!(fade.settled());
    assert_eq!(fade.arriving(), None, "the ground is painted plainly");
    assert!(fade.leaving().is_none());
    assert_eq!(
        fade.park_deadline_ns(BEGAN_NS, NO_DEADLINE_NS),
        NO_DEADLINE_NS,
        "an idle desktop arms no timer for a fade it is not running"
    );
}

#[test]
fn a_backdrop_dissolves_from_nothing_into_the_arriving_ground() {
    let mut fade = BackdropFade::default();
    fade.begin(BEGAN_NS, SPAN_MS, None);
    assert_eq!(
        fade.arriving(),
        Some(0),
        "the arriving ground starts invisible, so the first frame is what was there"
    );

    assert!(fade.advance(BEGAN_NS + SPAN_NS / 2));
    let midway = fade.arriving().expect("still dissolving");
    assert!(
        (100..=155).contains(&midway),
        "half way through the span is half way through the mix, got {midway}"
    );

    assert!(fade.advance(BEGAN_NS + SPAN_NS));
    assert!(
        fade.settled(),
        "the span ran out, so the ground has arrived"
    );
    assert_eq!(fade.arriving(), None);
}

#[test]
fn the_ground_being_left_is_released_once_the_fade_arrives() {
    // It is a screen-sized copy of a picture nobody can see any more; a
    // desktop that kept it would hold megabytes per wallpaper it ever showed.
    let mut fade = BackdropFade::default();
    fade.begin(BEGAN_NS, SPAN_MS, Some(ground()));
    assert!(
        fade.leaving().is_some(),
        "the ground being left is laid back over the arriving one"
    );

    fade.advance(BEGAN_NS + SPAN_NS / 2);
    assert!(fade.leaving().is_some(), "still part-way through");

    fade.advance(BEGAN_NS + SPAN_NS);
    assert!(fade.leaving().is_none());
}

#[test]
fn a_reduced_motion_theme_arrives_before_it_has_drawn_anything() {
    // Every duration is zero under reduced motion, which reads as "change it
    // now": the new ground is simply on screen, and no copy is held for a
    // fade that will never draw a frame.
    let mut fade = BackdropFade::default();
    fade.begin(BEGAN_NS, 0, Some(ground()));
    assert!(fade.settled());
    assert_eq!(fade.arriving(), None);
    assert!(fade.leaving().is_none());
    assert_eq!(
        fade.park_deadline_ns(BEGAN_NS, NO_DEADLINE_NS),
        NO_DEADLINE_NS
    );
}

#[test]
fn a_dissolving_backdrop_tightens_the_park_and_an_arrived_one_leaves_it() {
    let mut fade = BackdropFade::default();
    fade.begin(BEGAN_NS, SPAN_MS, None);
    let deadline = fade.park_deadline_ns(BEGAN_NS, NO_DEADLINE_NS);
    assert!(
        deadline > 0 && deadline < NO_DEADLINE_NS,
        "the desktop wakes for the next frame of the fade, got {deadline}"
    );

    fade.advance(BEGAN_NS + SPAN_NS);
    assert_eq!(
        fade.park_deadline_ns(BEGAN_NS + SPAN_NS, NO_DEADLINE_NS),
        NO_DEADLINE_NS,
        "an arrived fade hands the wait straight back"
    );
}

#[test]
fn advancing_an_arrived_fade_is_no_work_at_all() {
    let mut fade = BackdropFade::default();
    assert!(
        !fade.advance(BEGAN_NS),
        "nothing changed, so the desktop layer owes no repaint"
    );
    fade.begin(BEGAN_NS, SPAN_MS, None);
    fade.advance(BEGAN_NS + SPAN_NS);
    assert!(!fade.advance(BEGAN_NS + SPAN_NS * 2));
}

#[test]
fn a_clock_that_jumped_backwards_arrives_rather_than_stalling() {
    // The desktop would otherwise sit on a half-dissolved backdrop until the
    // clock caught up with where the fade began.
    let mut fade = BackdropFade::default();
    fade.begin(BEGAN_NS, SPAN_MS, Some(ground()));
    assert!(fade.advance(BEGAN_NS - SPAN_NS));
    assert!(fade.settled());
    assert!(fade.leaving().is_none());
}
