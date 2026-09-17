//! Unit tests for the seat's one tooltip: the dwell that shows it, the shared
//! placement rule that puts the plate on screen, and every reason it comes
//! down again.

use super::{SeatTooltip, TipSource, TOOLTIP_DWELL_NS};

use tairix_abi::window_ipc::WindowRegion;
use tairix_geometry::{Point, Rect, Scale};
use tairix_raster::Surface;
use tairix_theme::Theme;

/// The window every case declares for, and a second so a declaration can be
/// proved source-scoped.
///
/// Minted by a compositor because a window id names a real composited window:
/// the seat resolves a tip's region against the window it was declared on, so
/// an id no window holds could not be placed.
fn windows() -> (TipSource, TipSource) {
    let mut c = crate::tests::compositor();
    let one = c.add_window(Point::new(0, 0), Surface::new(1, 1).expect("a surface"));
    let two = c.add_window(Point::new(0, 0), Surface::new(1, 1).expect("a surface"));
    (TipSource::Window(one), TipSource::Window(two))
}

/// The screen the placement is bounded to.
const SCREEN: Rect = Rect::new(0, 0, 1024, 768);

/// Where the declaring window's client area begins on screen.
const ORIGIN: Point = Point::new(100, 80);

/// A region 40×20 client pixels in from the window's own origin.
fn region() -> WindowRegion {
    WindowRegion::new(40, 20, 60, 24).expect("a representable region")
}

/// The seam's answer when the seat can place every window: each begins at
/// [`ORIGIN`]. A function *pointer*, because the seam is legitimately
/// fallible — a window the seat cannot place answers `None`.
const ORIGINS: fn(TipSource) -> Option<Point> = |_| Some(ORIGIN);

/// A point inside the declared region, in screen coordinates.
fn inside() -> Point {
    Point::new(ORIGIN.x + 50, ORIGIN.y + 30)
}

/// A point outside it.
fn outside() -> Point {
    Point::new(ORIGIN.x + 500, ORIGIN.y + 400)
}

/// A seat with one window's declaration in place, and that window.
fn declared() -> (SeatTooltip, TipSource) {
    let (window, _) = windows();
    let mut tip = SeatTooltip::new();
    tip.declare(window, region(), "Copy the selection", Some(ORIGIN));
    (tip, window)
}

// --- The dwell ----------------------------------------------------------

#[test]
fn a_tip_opens_only_after_the_interval_and_only_inside_the_region() {
    let (mut tip, window) = declared();

    // A pointer merely arriving shows nothing: the rest is what asks.
    tip.pointer_moved(inside(), 1_000, ORIGINS);
    assert!(tip.is_dwelling());
    assert_eq!(tip.shown(), None);

    // Nor does a tick before the interval is out.
    assert!(!tip.tick(1_000 + TOOLTIP_DWELL_NS - 1));
    assert_eq!(tip.shown(), None);

    assert!(tip.tick(1_000 + TOOLTIP_DWELL_NS));
    assert_eq!(tip.shown(), Some(window));
    assert!(
        !tip.is_dwelling(),
        "the dwell is spent on the tip it opened"
    );
}

#[test]
fn a_pointer_outside_every_region_arms_nothing() {
    let (mut tip, _) = declared();
    assert!(!tip.pointer_moved(outside(), 0, ORIGINS));
    assert!(
        !tip.is_dwelling(),
        "nothing under the pointer asks for a tip"
    );
    assert!(!tip.tick(TOOLTIP_DWELL_NS * 4));
    assert_eq!(tip.shown(), None);
}

#[test]
fn resting_does_not_restart_the_countdown() {
    let (mut tip, window) = declared();
    tip.pointer_moved(inside(), 0, ORIGINS);
    // A stationary hand still produces samples; the delay is a rest, so the
    // deadline the first one set is the one that holds.
    for sample in 1..5 {
        tip.pointer_moved(inside(), sample, ORIGINS);
    }
    assert!(tip.tick(TOOLTIP_DWELL_NS));
    assert_eq!(tip.shown(), Some(window));
}

#[test]
fn the_park_is_shortened_to_the_moment_a_tip_is_due_and_no_further() {
    let (mut tip, _) = declared();
    let park = u64::MAX;
    assert_eq!(
        tip.park_deadline_ns(0, park),
        park,
        "an idle seat arms no timer: nothing wakes a core to watch a still pointer"
    );

    tip.pointer_moved(inside(), 1_000, ORIGINS);
    assert_eq!(
        tip.park_deadline_ns(1_000, park),
        TOOLTIP_DWELL_NS,
        "the park runs out exactly when the tip is due"
    );
    assert_eq!(
        tip.park_deadline_ns(1_000 + TOOLTIP_DWELL_NS + 5, park),
        0,
        "a deadline already past asks for no wait at all"
    );
}

// --- Coming down again --------------------------------------------------

#[test]
fn leaving_the_region_cancels_a_pending_dwell_and_closes_an_open_tip() {
    let (mut tip, _) = declared();
    tip.pointer_moved(inside(), 0, ORIGINS);
    assert!(!tip.pointer_moved(outside(), 1, ORIGINS));
    assert!(!tip.is_dwelling(), "the pending dwell is cancelled");
    assert!(!tip.tick(TOOLTIP_DWELL_NS * 4));

    tip.pointer_moved(inside(), 0, ORIGINS);
    assert!(tip.tick(TOOLTIP_DWELL_NS));
    assert!(
        tip.pointer_moved(outside(), TOOLTIP_DWELL_NS + 1, ORIGINS),
        "leaving the region takes the open tip down, which is a screen change"
    );
    assert_eq!(tip.shown(), None);
}

#[test]
fn travelling_within_the_region_the_tip_explains_changes_nothing() {
    let (mut tip, window) = declared();
    tip.pointer_moved(inside(), 0, ORIGINS);
    assert!(tip.tick(TOOLTIP_DWELL_NS));
    let elsewhere = Point::new(inside().x + 1, inside().y + 1);
    assert!(
        !tip.pointer_moved(elsewhere, TOOLTIP_DWELL_NS + 1, ORIGINS),
        "the tip already answers this pointer, so nothing is repainted"
    );
    assert_eq!(tip.shown(), Some(window));
}

#[test]
fn a_press_a_key_or_a_scroll_takes_the_tip_down() {
    // One answer to every event that ends a tip outright, so a caller need
    // not decide per event which of them means "no longer asking".
    let (mut tip, _) = declared();
    tip.pointer_moved(inside(), 0, ORIGINS);
    assert!(tip.tick(TOOLTIP_DWELL_NS));
    assert!(tip.dismiss(), "an open tip going down is a screen change");
    assert_eq!(tip.shown(), None);
    assert!(!tip.is_dwelling());

    // And a pending dwell is disarmed the same way, without a repaint: there
    // was nothing on screen to take down.
    tip.pointer_moved(inside(), 0, ORIGINS);
    assert!(!tip.dismiss());
    assert!(!tip.is_dwelling());
}

#[test]
fn a_withdrawn_declaration_closes_its_tip_and_shows_no_more() {
    let (mut tip, window) = declared();
    tip.pointer_moved(inside(), 0, ORIGINS);
    assert!(tip.tick(TOOLTIP_DWELL_NS));

    // Empty text is the withdrawal: one operation, and it takes the tip on
    // screen down with it.
    assert!(tip.declare(window, region(), "", Some(ORIGIN)));
    assert_eq!(tip.shown(), None);
    assert_eq!(tip.text(window), None);

    // And a dwell that was running for it resolves to nothing rather than to
    // a tip with no declaration behind it.
    tip.declare(window, region(), "Copy", Some(ORIGIN));
    tip.pointer_moved(inside(), 0, ORIGINS);
    assert!(tip.is_dwelling());
    assert!(
        !tip.withdraw(window),
        "nothing was on screen yet, so the withdrawal repaints nothing"
    );
    assert!(
        !tip.is_dwelling(),
        "the pending dwell went with the declaration it was counting for"
    );
    assert!(!tip.tick(TOOLTIP_DWELL_NS));
    assert_eq!(tip.shown(), None);
}

#[test]
fn a_dead_owner_takes_its_tip_with_it() {
    let (mut tip, window) = declared();
    tip.pointer_moved(inside(), 0, ORIGINS);
    assert!(tip.tick(TOOLTIP_DWELL_NS));
    assert!(tip.forget(window));
    assert_eq!(tip.shown(), None);
    assert_eq!(tip.text(window), None);
}

#[test]
fn a_re_declaration_replaces_rather_than_joining_and_restarts_the_dwell() {
    let (mut tip, window) = declared();
    tip.pointer_moved(inside(), 0, ORIGINS);
    assert!(tip.tick(TOOLTIP_DWELL_NS));
    assert_eq!(tip.text(window), Some("Copy the selection"));

    // The region may have moved under the tip, so the tip goes and the rest
    // starts again rather than a stale plate standing beside new pixels.
    assert!(tip.declare(window, region(), "Paste", Some(ORIGIN)));
    assert_eq!(tip.text(window), Some("Paste"));
    assert_eq!(tip.shown(), None);
}

#[test]
fn a_declaration_is_window_scoped() {
    let (window, other) = windows();
    let mut tip = SeatTooltip::new();
    tip.declare(window, region(), "Copy the selection", Some(ORIGIN));
    tip.declare(other, region(), "Other", Some(ORIGIN));
    assert_eq!(tip.text(window), Some("Copy the selection"));
    assert_eq!(tip.text(other), Some("Other"));

    // Withdrawing one leaves the other exactly as it was.
    tip.withdraw(other);
    assert_eq!(tip.text(window), Some("Copy the selection"));
    assert_eq!(tip.text(other), None);
}

// --- Placement ----------------------------------------------------------

#[test]
fn the_plate_is_placed_beside_the_region_and_stays_on_screen() {
    let theme = Theme::dark();
    let (mut tip, _) = declared();
    tip.pointer_moved(inside(), 0, ORIGINS);
    assert!(tip.tick(TOOLTIP_DWELL_NS));

    let (plate, rect) = tip
        .placed(SCREEN, Scale::ONE, &theme, ORIGINS)
        .expect("a shown tip is placed");
    assert_eq!(plate.text(), "Copy the selection");
    let anchor = Rect::new(ORIGIN.x + 40, ORIGIN.y + 20, 60, 24);
    assert!(
        rect.top() >= anchor.bottom(),
        "the plate hangs below the region it explains"
    );
    assert!(rect.left() >= SCREEN.left() && rect.right() <= SCREEN.right());
    assert!(rect.top() >= SCREEN.top() && rect.bottom() <= SCREEN.bottom());
}

#[test]
fn a_plate_at_every_screen_edge_stays_on_screen() {
    let theme = Theme::dark();
    let (window, _) = windows();
    for corner in [
        Point::new(0, 0),
        Point::new(SCREEN.right() - 4, 0),
        Point::new(0, SCREEN.bottom() - 4),
        Point::new(SCREEN.right() - 4, SCREEN.bottom() - 4),
    ] {
        let mut tip = SeatTooltip::new();
        tip.declare(
            window,
            WindowRegion::new(0, 0, 8, 8).expect("region"),
            "Tip",
            Some(corner),
        );
        let at = |_: TipSource| Some(corner);
        tip.pointer_moved(Point::new(corner.x + 1, corner.y + 1), 0, at);
        assert!(
            tip.tick(TOOLTIP_DWELL_NS),
            "a rest at {corner:?} shows a tip"
        );
        let (_, rect) = tip
            .placed(SCREEN, Scale::ONE, &theme, at)
            .expect("placed at every corner");
        assert!(
            rect.left() >= SCREEN.left()
                && rect.right() <= SCREEN.right()
                && rect.top() >= SCREEN.top()
                && rect.bottom() <= SCREEN.bottom(),
            "a plate at {corner:?} left the screen: {rect:?}"
        );
    }
}

#[test]
fn nothing_shown_is_placed_and_nothing_shown_draws() {
    let theme = Theme::dark();
    let (tip, _) = declared();
    assert!(tip.placed(SCREEN, Scale::ONE, &theme, ORIGINS).is_none());

    let mut surface = Surface::new(200, 60).expect("surface");
    let before = surface.pixels().to_vec();
    tip.render(&mut surface, Rect::new(0, 0, 200, 60), Scale::ONE, &theme);
    assert_eq!(
        surface.pixels(),
        before.as_slice(),
        "with no tip shown there is nothing to draw"
    );
}

#[test]
fn an_owner_whose_position_is_unknown_is_neither_placed_nor_hovered() {
    let theme = Theme::dark();
    let (mut tip, _) = declared();
    // A window the seat cannot locate cannot have its region resolved, so it
    // is not under the pointer and has nowhere to place a plate.
    let nowhere = |_: TipSource| None;
    assert!(!tip.pointer_moved(inside(), 0, nowhere));
    assert!(!tip.is_dwelling());

    tip.pointer_moved(inside(), 0, ORIGINS);
    assert!(tip.tick(TOOLTIP_DWELL_NS));
    assert!(
        tip.placed(SCREEN, Scale::ONE, &theme, nowhere).is_none(),
        "a plate is never placed against a position that is not known"
    );
}

#[test]
fn a_shown_tip_draws_its_declared_line() {
    let theme = Theme::dark();
    let (mut tip, _) = declared();
    tip.pointer_moved(inside(), 0, ORIGINS);
    assert!(tip.tick(TOOLTIP_DWELL_NS));

    let bounds = Rect::new(0, 0, 240, 40);
    let mut painted = Surface::new(bounds.width, bounds.height).expect("surface");
    tip.render(&mut painted, bounds, Scale::ONE, &theme);

    let mut reference = Surface::new(bounds.width, bounds.height).expect("surface");
    tairix_controls::Tooltip::new("Copy the selection").render(
        &mut reference,
        bounds,
        Scale::ONE,
        &theme,
    );
    assert_eq!(
        painted.pixels(),
        reference.pixels(),
        "the seat draws the shared control, never a plate of its own"
    );
}

// --- a declaration that lands under a pointer already at rest -----------

/// A pointer that has stopped sends no further sample, so a declaration
/// arriving *after* it must arm its own dwell — otherwise a tip is shown only
/// if the hand happens to jiggle.
///
/// This is the menu chain's ordinary case: the shell sees the motion sample
/// (it owns the tracked pointer), and only then does the chain move its
/// highlight and declare the row's explanation.
#[test]
fn a_declaration_under_a_resting_pointer_arms_its_own_dwell() {
    let (window, _) = windows();
    let mut tip = SeatTooltip::new();

    tip.pointer_moved(inside(), 1_000, ORIGINS);
    assert!(!tip.is_dwelling(), "nothing is declared to rest inside yet");

    assert!(!tip.declare(window, region(), "Copy", Some(ORIGIN)));
    assert!(
        tip.is_dwelling(),
        "the pointer is already resting inside it"
    );
    assert_eq!(
        tip.park_deadline_ns(1_000, u64::MAX),
        TOOLTIP_DWELL_NS,
        "and it is due a dwell after the pointer *stopped*, not after the \
         declaration happened to arrive"
    );
    assert!(tip.tick(1_000 + TOOLTIP_DWELL_NS));
    assert_eq!(tip.shown(), Some(window));
}

#[test]
fn a_declaration_the_resting_pointer_is_outside_arms_nothing() {
    let (window, _) = windows();
    let mut tip = SeatTooltip::new();
    tip.pointer_moved(outside(), 0, ORIGINS);
    assert!(!tip.declare(window, region(), "Copy", Some(ORIGIN)));
    assert!(!tip.is_dwelling());
    assert!(!tip.tick(TOOLTIP_DWELL_NS * 4));
    assert_eq!(tip.shown(), None);
}

#[test]
fn a_source_the_seat_cannot_place_arms_nothing_on_declaring() {
    let (window, _) = windows();
    let mut tip = SeatTooltip::new();
    tip.pointer_moved(inside(), 0, ORIGINS);
    // Fail closed: a region that resolves nowhere is not under the pointer.
    assert!(!tip.declare(window, region(), "Copy", None));
    assert!(!tip.is_dwelling());
    assert!(!tip.tick(TOOLTIP_DWELL_NS * 4));
}

/// A surface re-presenting an unchanged row declares the same thing again.
/// That is not a new declaration and must change nothing: taking the tip down
/// and counting again per frame would mean a tip that is due never falls due,
/// and one already up would blink.
#[test]
fn a_re_declaration_of_the_same_thing_changes_nothing() {
    let (window, _) = windows();
    let mut tip = SeatTooltip::new();
    tip.declare(window, region(), "Copy", Some(ORIGIN));
    tip.pointer_moved(inside(), 0, ORIGINS);
    assert!(tip.tick(TOOLTIP_DWELL_NS));
    assert_eq!(tip.shown(), Some(window));

    let before = tip.clone();
    assert!(
        !tip.declare(window, region(), "Copy", Some(ORIGIN)),
        "nothing on screen changed"
    );
    assert_eq!(tip, before, "and nothing in the seat did either");

    // A *changed* line is a new declaration and does take the tip down.
    assert!(tip.declare(window, region(), "Paste", Some(ORIGIN)));
    assert_eq!(tip.shown(), None);
}

/// A press ends the asking, so the rest goes with it: a *changed* declaration
/// over the same region must not pop a tip straight back up with no wait.
#[test]
fn a_dismissal_spends_the_rest_the_tip_was_due_on() {
    let (window, _) = windows();
    let mut tip = SeatTooltip::new();
    tip.declare(window, region(), "Copy", Some(ORIGIN));
    tip.pointer_moved(inside(), 0, ORIGINS);
    assert!(tip.tick(TOOLTIP_DWELL_NS));
    assert!(tip.dismiss());

    assert!(!tip.declare(window, region(), "Paste", Some(ORIGIN)));
    assert!(
        !tip.is_dwelling(),
        "the pointer has not moved since the press, so nothing is being asked"
    );
    assert!(!tip.tick(TOOLTIP_DWELL_NS * 4));
    assert_eq!(tip.shown(), None);

    // Moving again is asking again, and the wait starts from that move.
    tip.pointer_moved(inside(), TOOLTIP_DWELL_NS * 4, ORIGINS);
    assert!(tip.is_dwelling());
    assert!(!tip.tick(TOOLTIP_DWELL_NS * 4));
    assert!(tip.tick(TOOLTIP_DWELL_NS * 5));
    assert_eq!(tip.shown(), Some(window));
}
