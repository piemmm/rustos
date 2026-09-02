//! Host tests for what a round moved in the listing view.
//!
//! Both directions are proved: every pixel the browser draws differently after
//! a round lies inside what that round reported (or the frame left on screen
//! would be stale), and a round that moved one highlight reports exactly the
//! two entries it moved between (or "report everything" would pass too).

use alloc::vec::Vec;

use tairix_browse::render::{entry_rect, item_area, render_into, scrollbar_bounds, ManagerChrome};
use tairix_browse::{Browser, DirectorySource, ToolbarBand};
use tairix_controls::damage;
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_icon::NoArtwork;
use tairix_raster::Surface;
use tairix_theme::Theme;

use super::ViewMark;
use crate::test_fs::{filled, FilledFs};

/// The window the listing is laid out in for these tests.
const WINDOW: Rect = Rect::new(0, 0, 480, 320);

/// The chrome these tests measure against: the command band shown, which is
/// the layout with a header to offset the listing by.
const BAND: ToolbarBand = ToolbarBand::Shown;

/// A listing longer than the window can show, so the focus can be walked off
/// the bottom and scroll the view.
fn browser() -> Browser<FilledFs> {
    filled(60)
}

/// Paint `browser` into a fresh window-sized surface — one frame's pixels, for
/// comparing what a round actually moved against what it reported.
fn shot<S: DirectorySource>(browser: &Browser<S>) -> Surface {
    let mut surface = Surface::new(WINDOW.width, WINDOW.height).expect("surface");
    render_into(
        &mut surface,
        browser,
        Scale::ONE,
        &Theme::dark(),
        WINDOW,
        &ManagerChrome::none(),
        &mut NoArtwork,
    );
    surface
}

/// The first pixel where `before` and `after` differ that `damage` does not
/// name, or `None` when the report covered every change.
fn unreported_change(before: &Surface, after: &Surface, damage: &Region) -> Option<Point> {
    (WINDOW.top()..WINDOW.bottom()).find_map(|y| {
        (WINDOW.left()..WINDOW.right()).find_map(|x| {
            let (xu, yu) = (u32::try_from(x).ok()?, u32::try_from(y).ok()?);
            (before.get(xu, yu) != after.get(xu, yu) && !damage.contains(Point::new(x, y)))
                .then_some(Point::new(x, y))
        })
    })
}

/// The rectangle entry `index` is drawn in.
fn rect_of<S: DirectorySource>(browser: &Browser<S>, index: usize) -> Rect {
    entry_rect(browser, Scale::ONE, &Theme::dark(), WINDOW, BAND, index)
        .expect("the entry is on screen")
}

/// Report what `act` moved, answering the rectangles and whether it moved
/// anything.
fn round<S: DirectorySource>(
    browser: &mut Browser<S>,
    act: impl FnOnce(&mut Browser<S>),
) -> (bool, Region) {
    let mark = ViewMark::of(browser);
    let mut damage = damage::sink();
    act(browser);
    let moved = mark.report(
        browser,
        Scale::ONE,
        &Theme::dark(),
        WINDOW,
        BAND,
        &mut damage,
    );
    (moved, damage)
}

#[test]
fn a_focus_move_reports_the_entry_it_left_and_the_entry_it_reached() {
    let mut browser = browser();
    let (first, second) = (rect_of(&browser, 0), rect_of(&browser, 1));

    let (moved, damage) = round(&mut browser, Browser::select_next);

    assert!(moved);
    let mut want = damage::sink();
    want.add(first);
    want.add(second);
    assert_eq!(damage.rects(), want.rects());
}

#[test]
fn a_focus_that_cannot_move_reports_nothing() {
    let mut browser = browser();

    let (moved, damage) = round(&mut browser, Browser::select_previous);

    assert!(!moved, "the focus was already on the first entry");
    assert!(damage.is_empty());
}

#[test]
fn a_scroll_reports_every_entry_and_the_bar_beside_them() {
    let mut browser = browser();
    let theme = Theme::dark();

    let (moved, damage) = round(&mut browser, |browser| browser.set_scroll_offset(3));

    assert!(moved);
    let mut want = damage::sink();
    want.add(item_area(Scale::ONE, &theme, WINDOW));
    want.add(
        scrollbar_bounds(Scale::ONE, &theme, WINDOW, BAND).expect("a scrollable listing has a bar"),
    );
    assert_eq!(damage.rects(), want.rects());
}

#[test]
fn every_pixel_a_walk_moves_lies_inside_what_it_reported() {
    let mut browser = browser();
    let steps: Vec<fn(&mut Browser<FilledFs>)> = alloc::vec![
        Browser::select_next,
        Browser::select_next,
        Browser::select_previous,
        |browser| browser.set_scroll_offset(4),
        Browser::select_next,
        |browser| browser.set_scroll_offset(0),
    ];

    let mut moved_any = false;
    for (index, step) in steps.into_iter().enumerate() {
        let before = shot(&browser);
        let (_, damage) = round(&mut browser, step);
        let after = shot(&browser);
        assert_eq!(
            unreported_change(&before, &after, &damage),
            None,
            "step {index} moved a pixel it did not report"
        );
        moved_any |= before.pixels() != after.pixels();
    }
    assert!(moved_any, "the walk drew nothing new, so it proved nothing");
}

#[test]
fn a_focus_scrolled_out_of_view_still_covers_what_moved() {
    let mut browser = browser();
    // Walk the focus past the last visible row: keeping it on screen scrolls
    // the view, so every entry is drawn somewhere new.
    for _ in 0..40 {
        browser.select_next();
    }
    browser.set_scroll_offset(0);

    let before = shot(&browser);
    let (moved, damage) = round(&mut browser, |browser| {
        browser.set_scroll_offset(30);
    });
    let after = shot(&browser);

    assert!(moved);
    assert_eq!(unreported_change(&before, &after, &damage), None);
}
