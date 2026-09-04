//! Headless tests for the settings sheet's retained picture.
//!
//! The property under test is the one a slider drag depends on: a paint covers
//! the rectangle the sheet's controls reported and no more, while everything a
//! control could *not* have reported still covers the sheet — so a scoped
//! paint can never leave a stale pixel on screen. What a real drag reports is
//! [`crate::settings`]'s own to assert.

use tairix_controls::damage;
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_raster::Surface;
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

/// Drive `gesture` into a sheet through the screen's own sink, paint what it
/// reported, and assert the retained picture now holds what a whole repaint of
/// the same sheet would.
///
/// This is the property every report exists to keep, and the only one that
/// catches an under-report: a change the sheet made but did not name shows up
/// here as pixels the screen is still showing and should not be.
fn reports_cover_the_gesture(
    label: &str,
    gesture: impl Fn(&mut Settings, Rect, &Theme, &mut Region),
) {
    let theme = theme();
    let (mut scoped, mut sheet, viewport) = opened();
    assert_eq!(scoped.paint(&sheet, viewport, SCALE, &theme), viewport);

    gesture(&mut sheet, viewport, &theme, scoped.sink());
    scoped.paint(&sheet, viewport, SCALE, &theme);

    // A picture with nothing on it yet paints the sheet the gesture left,
    // whole, so it is what the screen ought to be showing.
    let (mut whole, ..) = opened();
    assert_eq!(whole.paint(&sheet, viewport, SCALE, &theme), viewport);
    assert_eq!(
        stale_area(scoped.surface(), whole.surface()),
        Rect::EMPTY,
        "{label}: the scoped paint left pixels a whole one would not"
    );
}

/// The rectangle enclosing every pixel of `shown` that differs from `owed`, so
/// a failure names where the missing report was rather than dumping two
/// pictures.
fn stale_area(shown: &Surface, owed: &Surface) -> Rect {
    let width = shown.width().min(owed.width());
    if width == 0 {
        return Rect::EMPTY;
    }
    let mut area = damage::sink();
    for (index, (a, b)) in shown.pixels().iter().zip(owed.pixels()).enumerate() {
        if a == b {
            continue;
        }
        let Ok(index) = u32::try_from(index) else {
            break;
        };
        let (Ok(x), Ok(y)) = (i32::try_from(index % width), i32::try_from(index / width)) else {
            break;
        };
        area.add(Rect::new(x, y, 1, 1));
    }
    area.bounds()
}

/// One key press with no modifiers.
fn press_key(
    sheet: &mut Settings,
    viewport: Rect,
    theme: &Theme,
    damage: &mut Region,
    key: NamedKey,
) {
    sheet.on_key(
        Key::Named(key),
        Modifiers::default(),
        viewport,
        SCALE,
        theme,
        damage,
    );
}

#[test]
fn switching_tabs_repaints_the_body_it_replaced() {
    reports_cover_the_gesture("keyboard tab switch", |sheet, viewport, theme, damage| {
        // Focus opens on the tab strip, so this moves the cursor to the next
        // tab and chooses it without disturbing anything else.
        press_key(sheet, viewport, theme, damage, NamedKey::Right);
        press_key(sheet, viewport, theme, damage, NamedKey::Enter);
    });
}

#[test]
fn walking_the_focus_order_repaints_the_rings_it_moves() {
    reports_cover_the_gesture("tab traversal", |sheet, viewport, theme, damage| {
        for _ in 0..4 {
            press_key(sheet, viewport, theme, damage, NamedKey::Tab);
        }
    });
}

#[test]
fn editing_from_the_keyboard_repaints_the_label_beside_the_control() {
    reports_cover_the_gesture("keyed edit", |sheet, viewport, theme, damage| {
        // Past the strip onto the first row, then along the focus order
        // editing whatever each row offers `End`.
        for _ in 0..3 {
            press_key(sheet, viewport, theme, damage, NamedKey::Tab);
            press_key(sheet, viewport, theme, damage, NamedKey::End);
        }
    });
}

#[test]
fn every_press_the_sheet_claims_repaints_what_it_changed() {
    // A press-drag-release at each node of a grid over the whole sheet, each
    // on a sheet of its own: a lattice this fine lands on every band, every
    // row, and every gap between them, so no path is asserted only in the
    // abstract. The drag is what makes a slider commit a value it must then
    // redraw its label for.
    const STEP: u32 = 12;

    let (_, _, viewport) = opened();
    for down in 0..viewport.height / STEP {
        for across in 0..viewport.width / STEP {
            let (Ok(x), Ok(y)) = (i32::try_from(across * STEP), i32::try_from(down * STEP)) else {
                continue;
            };
            let at = Point::new(viewport.left() + x, viewport.top() + y);
            let drag = i32::try_from(STEP * 2).unwrap_or(0);
            reports_cover_the_gesture(
                &alloc::format!("press at {at:?}"),
                |sheet, viewport, theme, damage| {
                    for event in [
                        InputEvent::PointerMoved { to: at },
                        InputEvent::PointerPressed {
                            button: PointerButton::Primary,
                        },
                        // Diagonal and both ways, so one drag exercises a
                        // horizontal control's travel and a vertical bar's
                        // alike, and neither is left clamped at an end it
                        // started on.
                        InputEvent::PointerMoved {
                            to: Point::new(at.x - drag, at.y - drag),
                        },
                        InputEvent::PointerMoved {
                            to: Point::new(at.x + drag, at.y + drag),
                        },
                        InputEvent::PointerReleased {
                            button: PointerButton::Primary,
                        },
                    ] {
                        sheet.on_pointer(&event, viewport, SCALE, theme, damage);
                    }
                },
            );
        }
    }
}
