//! Headless tests for the settings sheet's retained picture.
//!
//! The property under test is the one a slider drag depends on: a paint covers
//! the rectangle the sheet's controls reported and no more, while everything a
//! control could *not* have reported still covers the sheet — so a scoped
//! paint can never leave a stale pixel on screen. What a real drag reports is
//! [`crate::settings`]'s own to assert.

use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, PointerButton};
use tairix_theme::Theme;

use super::SheetScreen;
use crate::profile::Profile;
use crate::settings::{preferred_extent, Settings};

const SCALE: Scale = Scale::ONE;

/// The sheet in a popup of exactly its own preferred extent — the popup the
/// terminal opens it in.
fn opened() -> (SheetScreen, Settings, Rect) {
    let (w, h) = preferred_extent(SCALE);
    let screen = SheetScreen::new(w, h).expect("the sheet's own extent allocates");
    (
        screen,
        Settings::new(&Profile::default()),
        Rect::new(0, 0, w, h),
    )
}

fn theme() -> Theme {
    Theme::dark()
}

#[test]
fn the_first_paint_covers_the_whole_sheet() {
    let (mut screen, sheet, viewport) = opened();
    assert_eq!(
        screen.paint(&sheet, viewport, SCALE, &theme()),
        viewport,
        "nothing is on screen yet, so no report could describe the frame"
    );
}

#[test]
fn a_paint_with_nothing_reported_draws_nothing() {
    let (mut screen, sheet, viewport) = opened();
    assert_eq!(screen.paint(&sheet, viewport, SCALE, &theme()), viewport);
    assert_eq!(
        screen.paint(&sheet, viewport, SCALE, &theme()),
        Rect::EMPTY,
        "an unchanged sheet costs no pixels and presents nothing"
    );
}

#[test]
fn an_edit_repaints_exactly_what_its_controls_reported() {
    let (mut screen, mut sheet, viewport) = opened();
    let theme = theme();
    assert_eq!(screen.paint(&sheet, viewport, SCALE, &theme), viewport);

    // Route a real gesture into the sheet through the screen's own sink, so
    // what is painted is what the controls said, not what a test decided.
    // The sheet's row geometry is its own business, so this walks the centre
    // line until a press lands on something rather than restating it.
    let x = viewport.left() + i32::try_from(viewport.width / 2).expect("a sane extent");
    let mut hit = false;
    for step in 0..viewport.height / 8 {
        let at = Point::new(
            x,
            viewport.top() + i32::try_from(step * 8).expect("a sane extent"),
        );
        let reported = {
            let damage = screen.sink();
            sheet.on_pointer(
                &InputEvent::PointerMoved { to: at },
                viewport,
                SCALE,
                &theme,
                damage,
            );
            sheet.on_pointer(
                &InputEvent::PointerPressed {
                    button: PointerButton::Primary,
                },
                viewport,
                SCALE,
                &theme,
                damage,
            );
            sheet.on_pointer(
                &InputEvent::PointerReleased {
                    button: PointerButton::Primary,
                },
                viewport,
                SCALE,
                &theme,
                damage,
            );
            damage.bounds()
        };
        let painted = screen.paint(&sheet, viewport, SCALE, &theme);
        assert_eq!(
            painted,
            reported.intersection(&viewport),
            "the paint is the report, clipped to the sheet"
        );
        if !reported.is_empty() {
            hit = true;
            assert!(
                painted.width < viewport.width || painted.height < viewport.height,
                "one control's report must not cover the whole sheet: \
                 {painted:?} of {viewport:?}"
            );
        }
    }
    assert!(hit, "some control on the centre line must report a press");
}

#[test]
fn an_invalidated_sheet_covers_itself_again() {
    let (mut screen, sheet, viewport) = opened();
    let theme = theme();
    assert_eq!(screen.paint(&sheet, viewport, SCALE, &theme), viewport);
    assert_eq!(screen.paint(&sheet, viewport, SCALE, &theme), Rect::EMPTY);

    // A re-theme, a new scale, a profile adopted from the store, a frame
    // region the session took back: no control reported any of them, so the
    // whole sheet is owed.
    screen.invalidate();
    assert_eq!(screen.paint(&sheet, viewport, SCALE, &theme), viewport);
}

#[test]
fn reports_accumulate_across_the_batch_that_made_them() {
    let (mut screen, sheet, viewport) = opened();
    let theme = theme();
    assert_eq!(screen.paint(&sheet, viewport, SCALE, &theme), viewport);

    // A drained batch of pointer samples reports repeatedly before one paint,
    // so the paint must cover every rectangle in it.
    screen.sink().add(Rect::new(4, 4, 10, 10));
    screen.sink().add(Rect::new(40, 60, 10, 10));
    let painted = screen.paint(&sheet, viewport, SCALE, &theme);
    assert!(painted.contains(Point::new(4, 4)));
    assert!(painted.contains(Point::new(49, 69)));
}

#[test]
fn a_scoped_paint_leaves_the_picture_a_whole_one_would() {
    let theme = theme();
    let (mut whole, sheet, viewport) = opened();
    let (mut scoped, _, _) = opened();
    assert_eq!(whole.paint(&sheet, viewport, SCALE, &theme), viewport);
    assert_eq!(scoped.paint(&sheet, viewport, SCALE, &theme), viewport);

    // Repaint a band of one and the whole of the other: the pictures must be
    // byte-identical, or a scoped present would show something a whole one
    // would not.
    let band = Rect::new(0, 20, viewport.width, 40);
    scoped.sink().add(band);
    assert_eq!(scoped.paint(&sheet, viewport, SCALE, &theme), band);
    whole.invalidate();
    assert_eq!(whole.paint(&sheet, viewport, SCALE, &theme), viewport);
    assert_eq!(whole.surface().pixels(), scoped.surface().pixels());
}
