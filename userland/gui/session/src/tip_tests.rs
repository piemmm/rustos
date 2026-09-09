//! Unit tests for the seat's one tooltip: the dwell that shows it, the shared
//! placement rule that puts the plate on screen, and every reason it comes
//! down again.

use super::{SeatTooltip, TOOLTIP_DWELL_NS};

use tairix_abi::window_ipc::WindowRegion;
use tairix_geometry::{Point, Rect, Scale};
use tairix_raster::Surface;
use tairix_theme::Theme;

/// The window every case declares for.
const WINDOW: u64 = 7;
/// A second window, so a declaration can be proved window-scoped.
const OTHER: u64 = 9;

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
const ORIGINS: fn(u64) -> Option<Point> = |_| Some(ORIGIN);

/// A point inside the declared region, in screen coordinates.
fn inside() -> Point {
    Point::new(ORIGIN.x + 50, ORIGIN.y + 30)
}

/// A point outside it.
fn outside() -> Point {
    Point::new(ORIGIN.x + 500, ORIGIN.y + 400)
}

/// A seat with [`WINDOW`]'s declaration in place.
fn declared() -> SeatTooltip {
    let mut tip = SeatTooltip::new();
    tip.declare(WINDOW, region(), "Copy the selection");
    tip
}

// --- The dwell ----------------------------------------------------------

#[test]
fn a_tip_opens_only_after_the_interval_and_only_inside_the_region() {
    let mut tip = declared();

    // A pointer merely arriving shows nothing: the rest is what asks.
    tip.pointer_moved(inside(), 1_000, ORIGINS);
    assert!(tip.is_dwelling());
    assert_eq!(tip.shown(), None);

    // Nor does a tick before the interval is out.
    assert!(!tip.tick(1_000 + TOOLTIP_DWELL_NS - 1));
    assert_eq!(tip.shown(), None);

    assert!(tip.tick(1_000 + TOOLTIP_DWELL_NS));
    assert_eq!(tip.shown(), Some(WINDOW));
    assert!(
        !tip.is_dwelling(),
        "the dwell is spent on the tip it opened"
    );
}

#[test]
fn a_pointer_outside_every_region_arms_nothing() {
    let mut tip = declared();
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
    let mut tip = declared();
    tip.pointer_moved(inside(), 0, ORIGINS);
    // A stationary hand still produces samples; the delay is a rest, so the
    // deadline the first one set is the one that holds.
    for sample in 1..5 {
        tip.pointer_moved(inside(), sample, ORIGINS);
    }
    assert!(tip.tick(TOOLTIP_DWELL_NS));
    assert_eq!(tip.shown(), Some(WINDOW));
}

#[test]
fn the_park_is_shortened_to_the_moment_a_tip_is_due_and_no_further() {
    let mut tip = declared();
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
    let mut tip = declared();
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
    let mut tip = declared();
    tip.pointer_moved(inside(), 0, ORIGINS);
    assert!(tip.tick(TOOLTIP_DWELL_NS));
    let elsewhere = Point::new(inside().x + 1, inside().y + 1);
    assert!(
        !tip.pointer_moved(elsewhere, TOOLTIP_DWELL_NS + 1, ORIGINS),
        "the tip already answers this pointer, so nothing is repainted"
    );
    assert_eq!(tip.shown(), Some(WINDOW));
}

#[test]
fn a_press_a_key_or_a_scroll_takes_the_tip_down() {
    // One answer to every event that ends a tip outright, so a caller need
    // not decide per event which of them means "no longer asking".
    let mut tip = declared();
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
    let mut tip = declared();
    tip.pointer_moved(inside(), 0, ORIGINS);
    assert!(tip.tick(TOOLTIP_DWELL_NS));

    // Empty text is the withdrawal: one operation, and it takes the tip on
    // screen down with it.
    assert!(tip.declare(WINDOW, region(), ""));
    assert_eq!(tip.shown(), None);
    assert_eq!(tip.text(WINDOW), None);

    // And a dwell that was running for it resolves to nothing rather than to
    // a tip with no declaration behind it.
    tip.declare(WINDOW, region(), "Copy");
    tip.pointer_moved(inside(), 0, ORIGINS);
    assert!(tip.is_dwelling());
    assert!(
        !tip.withdraw(WINDOW),
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
    let mut tip = declared();
    tip.pointer_moved(inside(), 0, ORIGINS);
    assert!(tip.tick(TOOLTIP_DWELL_NS));
    assert!(tip.forget(WINDOW));
    assert_eq!(tip.shown(), None);
    assert_eq!(tip.text(WINDOW), None);
}

#[test]
fn a_re_declaration_replaces_rather_than_joining_and_restarts_the_dwell() {
    let mut tip = declared();
    tip.pointer_moved(inside(), 0, ORIGINS);
    assert!(tip.tick(TOOLTIP_DWELL_NS));
    assert_eq!(tip.text(WINDOW), Some("Copy the selection"));

    // The region may have moved under the tip, so the tip goes and the rest
    // starts again rather than a stale plate standing beside new pixels.
    assert!(tip.declare(WINDOW, region(), "Paste"));
    assert_eq!(tip.text(WINDOW), Some("Paste"));
    assert_eq!(tip.shown(), None);
}

#[test]
fn a_declaration_is_window_scoped() {
    let mut tip = declared();
    tip.declare(OTHER, region(), "Other");
    assert_eq!(tip.text(WINDOW), Some("Copy the selection"));
    assert_eq!(tip.text(OTHER), Some("Other"));

    // Withdrawing one leaves the other exactly as it was.
    tip.withdraw(OTHER);
    assert_eq!(tip.text(WINDOW), Some("Copy the selection"));
    assert_eq!(tip.text(OTHER), None);
}

// --- Placement ----------------------------------------------------------

#[test]
fn the_plate_is_placed_beside_the_region_and_stays_on_screen() {
    let theme = Theme::dark();
    let mut tip = declared();
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
    for corner in [
        Point::new(0, 0),
        Point::new(SCREEN.right() - 4, 0),
        Point::new(0, SCREEN.bottom() - 4),
        Point::new(SCREEN.right() - 4, SCREEN.bottom() - 4),
    ] {
        let mut tip = SeatTooltip::new();
        tip.declare(
            WINDOW,
            WindowRegion::new(0, 0, 8, 8).expect("region"),
            "Tip",
        );
        let at = |_: u64| Some(corner);
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
    let tip = declared();
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
    let mut tip = declared();
    // A window the seat cannot locate cannot have its region resolved, so it
    // is not under the pointer and has nowhere to place a plate.
    let nowhere = |_: u64| None;
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
    let mut tip = declared();
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
