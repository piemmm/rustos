//! The frame counters, and the proof that the opaque-run copy path composes
//! the same bytes the general blend does.
//!
//! The counters are asserted exactly rather than as bounds: a scene's work is
//! a function of the scene, so an exact expectation is reproducible under any
//! machine load, while a bound would hide the regression it exists to catch.
//!
//! The fixtures come from the sibling test module rather than a second set of
//! their own, so a compositor built here is the one every other test builds.

extern crate alloc;

use alloc::vec::Vec;

use crate::color::Color;
use crate::corner::Corners;
use crate::geometry::{Point, Rect};
use crate::tests::{mode, new_compositor, opaque, solid_cursor, MockDisplay};
use crate::{Compositor, FrameStats, WindowId};
use tairix_abi::sysinfo::DesktopFrameTotals;

/// A 64×48 compositor over a near-black root fill: small enough that a whole
/// screen of expected pixel counts is readable in an assertion.
fn compositor() -> Compositor {
    new_compositor(mode(64, 48), Color::rgb(8, 8, 8)).expect("a small compositor allocates")
}

fn add(comp: &mut Compositor, x: i32, y: i32, w: u32, h: u32, color: Color) -> WindowId {
    comp.add_window(Point::new(x, y), opaque(w, h, color))
}

/// Compose the whole-screen damage a fresh compositor starts with and hand
/// back the scan-out bytes.
fn first_frame(comp: &mut Compositor) -> Vec<u8> {
    comp.composite();
    comp.frame().to_vec()
}

/// Compose `scene` twice — once with the opaque-run copy path allowed and once
/// without it — and assert the scan-out bytes and the back buffer are
/// identical.
///
/// This is the whole bit-identity argument for the fast path: the general
/// blend is the reference, and a run that copies must land on exactly the bytes
/// it would otherwise have blended.
fn assert_run_path_is_exact(name: &str, scene: impl Fn(&mut Compositor)) {
    let mut fast = compositor();
    scene(&mut fast);
    let fast_frame = first_frame(&mut fast);
    let fast_back = fast.back_buffer().pixels().to_vec();

    let mut slow = compositor();
    slow.set_opaque_runs(false);
    scene(&mut slow);
    let slow_frame = first_frame(&mut slow);

    assert_eq!(
        fast_frame, slow_frame,
        "{name}: the opaque-run path changed the scan-out frame"
    );
    assert_eq!(
        fast_back,
        slow.back_buffer().pixels(),
        "{name}: the opaque-run path changed the back buffer"
    );
    assert_eq!(
        slow.frame_stats().opaque_px,
        0,
        "{name}: the reference walk must copy nothing"
    );
}

#[test]
fn an_opaque_window_over_the_root_fill_composes_identically() {
    assert_run_path_is_exact("opaque", |comp| {
        add(comp, 8, 8, 32, 24, Color::rgb(200, 30, 30));
    });
}

#[test]
fn a_translucent_window_composes_identically() {
    assert_run_path_is_exact("translucent", |comp| {
        let id = add(comp, 4, 4, 40, 30, Color::rgb(0, 0, 200));
        comp.set_opacity(id, 200);
    });
}

#[test]
fn a_rounded_window_composes_identically() {
    assert_run_path_is_exact("rounded", |comp| {
        let id = add(comp, 6, 6, 30, 20, Color::rgb(20, 180, 90));
        comp.set_corners(id, Corners::Rounded { radius: 6 });
    });
}

#[test]
fn a_stack_of_overlapping_windows_composes_identically() {
    assert_run_path_is_exact("stack", |comp| {
        add(comp, 0, 0, 64, 48, Color::rgb(10, 10, 90));
        let middle = add(comp, 8, 8, 30, 30, Color::rgb(200, 200, 0));
        comp.set_opacity(middle, 180);
        add(comp, 20, 12, 20, 20, Color::rgb(240, 240, 240));
    });
}

#[test]
fn a_desktop_layer_under_an_opaque_window_composes_identically() {
    assert_run_path_is_exact("desktop", |comp| {
        comp.set_desktop(opaque(64, 48, Color::rgb(60, 40, 20)));
        add(comp, 10, 10, 24, 18, Color::rgb(0, 0, 0));
    });
}

#[test]
fn a_blurred_window_composes_identically() {
    assert_run_path_is_exact("blurred", |comp| {
        comp.set_desktop(opaque(64, 48, Color::rgb(60, 40, 20)));
        add(comp, 4, 4, 20, 16, Color::rgb(240, 0, 0));
        let glass = add(comp, 12, 10, 30, 24, Color::rgb(255, 255, 255));
        comp.set_opacity(glass, 90);
        comp.set_backdrop_blur(glass, 3);
    });
}

#[test]
fn a_window_partly_off_screen_composes_identically() {
    assert_run_path_is_exact("off-screen", |comp| {
        add(comp, -10, -6, 32, 24, Color::rgb(0, 200, 200));
        add(comp, 50, 38, 32, 24, Color::rgb(200, 0, 200));
    });
}

#[test]
fn a_cursor_row_composes_identically() {
    assert_run_path_is_exact("cursor", |comp| {
        add(comp, 0, 0, 64, 48, Color::rgb(30, 30, 30));
        comp.set_cursor(solid_cursor(4, Color::rgb(255, 0, 0)), Point::new(20, 20));
    });
}

#[test]
fn one_step_below_full_opacity_takes_no_run() {
    let mut comp = compositor();
    let id = add(&mut comp, 0, 0, 64, 48, Color::rgb(90, 90, 90));
    comp.set_opacity(id, 254);
    first_frame(&mut comp);
    assert_eq!(
        comp.frame_stats().opaque_px,
        0,
        "one step below full opacity scales every pixel, so no run may copy"
    );
    assert_run_path_is_exact("opacity 254", |comp| {
        let id = add(comp, 0, 0, 64, 48, Color::rgb(90, 90, 90));
        comp.set_opacity(id, 254);
    });
}

#[test]
fn a_fade_in_flight_takes_no_run_and_still_composes_identically() {
    let mut comp = compositor();
    add(&mut comp, 0, 0, 64, 48, Color::rgb(90, 90, 90));
    comp.set_reveal(128);
    first_frame(&mut comp);
    assert_eq!(
        comp.frame_stats().opaque_px,
        0,
        "the reveal is applied as a pixel is encoded, so a copy cannot serve it"
    );
    assert_run_path_is_exact("fade", |comp| {
        add(comp, 0, 0, 64, 48, Color::rgb(90, 90, 90));
        comp.set_reveal(128);
    });
}

#[test]
fn a_one_pixel_repaint_composes_identically() {
    let repaint = |comp: &mut Compositor, id: WindowId| {
        comp.present_window_content(id, 64, 48, |surface| {
            surface.set(31, 17, Color::rgb(1, 2, 3).premultiply());
            ((), Rect::new(31, 17, 1, 1))
        });
        comp.composite();
    };

    let mut fast = compositor();
    let id = add(&mut fast, 0, 0, 64, 48, Color::rgb(77, 88, 99));
    first_frame(&mut fast);
    repaint(&mut fast, id);
    let stats = fast.frame_stats();
    assert_eq!(stats.damaged_px, 1, "one pixel changed: {stats:?}");
    let fast_frame = fast.frame().to_vec();

    let mut slow = compositor();
    slow.set_opaque_runs(false);
    let id = add(&mut slow, 0, 0, 64, 48, Color::rgb(77, 88, 99));
    first_frame(&mut slow);
    repaint(&mut slow, id);

    assert_eq!(fast_frame, slow.frame().to_vec());
}

#[test]
fn a_frame_that_recomposed_nothing_counts_nothing() {
    let mut comp = compositor();
    first_frame(&mut comp);
    comp.composite();
    let stats = comp.frame_stats();
    assert!(stats.is_idle(), "an undamaged frame is idle: {stats:?}");
    assert_eq!(stats, FrameStats::ZERO);
}

#[test]
fn a_move_counts_the_two_rectangles_it_dirtied() {
    let mut comp = compositor();
    let id = add(&mut comp, 0, 0, 10, 8, Color::rgb(9, 9, 9));
    first_frame(&mut comp);
    comp.move_window(id, Point::new(40, 30));
    comp.composite();
    let stats = comp.frame_stats();
    assert_eq!(
        stats.dirty_rects, 2,
        "the rectangle left and the one entered"
    );
    assert_eq!(stats.damaged_px, 2 * 10 * 8);
    assert_eq!(
        stats.encoded_px, stats.damaged_px,
        "every damaged pixel is encoded exactly once"
    );
}

#[test]
fn damage_wholly_off_the_screen_costs_nothing() {
    let mut comp = compositor();
    let id = add(&mut comp, 200, 200, 10, 10, Color::rgb(9, 9, 9));
    first_frame(&mut comp);
    comp.set_opacity(id, 128);
    comp.composite();
    assert!(comp.frame_stats().is_idle());
}

#[test]
fn a_full_screen_opaque_window_blends_nothing_beneath_itself() {
    let mut comp = compositor();
    comp.set_desktop(opaque(64, 48, Color::rgb(60, 40, 20)));
    add(&mut comp, 0, 0, 64, 48, Color::rgb(5, 5, 5));
    first_frame(&mut comp);
    let stats = comp.frame_stats();
    assert_eq!(stats.damaged_px, 64 * 48);
    assert_eq!(
        stats.opaque_px, stats.damaged_px,
        "every pixel came from the covering window: {stats:?}"
    );
    assert_eq!(
        stats.blended_px, 0,
        "the desktop layer and the root fill were never blended: {stats:?}"
    );
}

#[test]
fn a_covered_window_contributes_no_blending() {
    let mut comp = compositor();
    add(&mut comp, 10, 10, 20, 20, Color::rgb(200, 0, 0));
    add(&mut comp, 0, 0, 64, 48, Color::rgb(5, 5, 5));
    first_frame(&mut comp);
    assert_eq!(comp.frame_stats().blended_px, 0);
}

#[test]
fn a_translucent_window_blends_every_pixel_it_covers() {
    let mut comp = compositor();
    let id = add(&mut comp, 0, 0, 64, 48, Color::rgb(200, 0, 0));
    comp.set_opacity(id, 128);
    first_frame(&mut comp);
    let stats = comp.frame_stats();
    assert_eq!(stats.opaque_px, 0);
    assert_eq!(stats.blended_px, 64 * 48);
}

#[test]
fn a_frosted_window_counts_the_pixels_it_frosted() {
    let mut comp = compositor();
    let glass = add(&mut comp, 8, 6, 20, 16, Color::rgb(255, 255, 255));
    comp.set_opacity(glass, 90);
    comp.set_backdrop_blur(glass, 2);
    first_frame(&mut comp);
    assert_eq!(comp.frame_stats().blur_px, 20 * 16);
}

#[test]
fn an_unfrosted_frame_counts_no_blur() {
    let mut comp = compositor();
    add(&mut comp, 0, 0, 64, 48, Color::rgb(1, 2, 3));
    first_frame(&mut comp);
    assert_eq!(comp.frame_stats().blur_px, 0);
}

/// The epoch totals as the ABI decoder would hand them back.
///
/// The bounds `DesktopFrameTotals::from_bytes` enforces are claims about what
/// a composite pass can produce, so round-tripping the compositor's own fold
/// through the wire is the proof they hold — a producer the receiver would
/// reject is a defect on this side.
fn decoded(totals: DesktopFrameTotals) -> Result<DesktopFrameTotals, tairix_abi::Errno> {
    DesktopFrameTotals::from_bytes(&totals.to_le_bytes())
}

#[test]
fn a_compositor_that_has_composed_nothing_reports_an_empty_epoch() {
    let comp = compositor();
    let totals = comp.frame_totals();
    assert_eq!(totals, DesktopFrameTotals::ZERO);
    // The one shape the epoch reader must never confuse: no frame at all,
    // versus one frame that counted nothing.
    assert_eq!(totals.frames, 0);
}

#[test]
fn one_frame_that_counted_nothing_is_still_a_frame() {
    let mut comp = compositor();
    first_frame(&mut comp);
    comp.composite();
    let totals = comp.frame_totals();
    assert_eq!(totals.frames, 2, "the opening frame and the idle one");
    assert_eq!(totals.damaged_px, 64 * 48, "only the opening frame damaged");
}

#[test]
fn the_epoch_sums_the_frames_and_keeps_the_worst_of_them() {
    let mut comp = compositor();
    let id = add(&mut comp, 0, 0, 10, 8, Color::rgb(9, 9, 9));
    // A whole-screen opening frame, then a two-rectangle move, then a
    // one-rectangle repaint: three frames of decreasing damage, so the peak
    // is the first and neither the last nor the sum.
    first_frame(&mut comp);
    comp.move_window(id, Point::new(40, 30));
    comp.composite();
    comp.set_opacity(id, 200);
    comp.composite();

    let totals = comp.frame_totals();
    assert_eq!(totals.frames, 3);
    assert_eq!(totals.screen_px, 64 * 48);
    assert_eq!(totals.damaged_px, 64 * 48 + 2 * 10 * 8 + 10 * 8);
    assert_eq!(totals.dirty_rects, 1 + 2 + 1);
    assert_eq!(
        totals.peak_damaged_px,
        64 * 48,
        "the worst frame, not the last: {totals:?}"
    );
    assert_eq!(
        decoded(totals),
        Ok(totals),
        "what the compositor folds must pass the decoder's gate"
    );
}

#[test]
fn reading_the_epoch_twice_counts_the_open_frame_once() {
    let mut comp = compositor();
    let id = add(&mut comp, 0, 0, 10, 8, Color::rgb(9, 9, 9));
    first_frame(&mut comp);
    comp.move_window(id, Point::new(20, 20));
    comp.composite();

    let once = comp.frame_totals();
    let twice = comp.frame_totals();
    assert_eq!(once, twice, "the read must be pure");
    // And the next frame folds the open one exactly once more.
    comp.composite();
    assert_eq!(comp.frame_totals().frames, once.frames + 1);
}

#[test]
fn a_present_is_counted_in_the_epoch_it_published() {
    let mut comp = compositor();
    add(&mut comp, 0, 0, 10, 8, Color::rgb(9, 9, 9));
    let mut display = MockDisplay::new(mode(64, 48));
    comp.present(&mut display)
        .expect("the mock present accepts");
    let totals = comp.frame_totals();
    assert_eq!(totals.frames, 1);
    assert_eq!(totals.present_calls, 1);
    assert_eq!(
        totals.encoded_px, totals.damaged_px,
        "every damaged pixel was encoded once: {totals:?}"
    );
    assert_eq!(decoded(totals), Ok(totals));
}

#[test]
fn a_frame_of_every_kind_of_work_still_satisfies_the_decoder() {
    // The bounds the ABI decoder enforces are claims about what a composite
    // pass can produce, so the scene that exercises every counter — an
    // opaque cover, a translucent window, a frosted one, furniture, a
    // cursor, and a real present — is what proves they hold.
    let mut comp = compositor();
    comp.set_desktop(opaque(64, 48, Color::rgb(60, 40, 20)));
    add(&mut comp, 0, 0, 30, 24, Color::rgb(5, 5, 5));
    let glass = add(&mut comp, 8, 6, 20, 16, Color::rgb(255, 255, 255));
    comp.set_opacity(glass, 90);
    comp.set_backdrop_blur(glass, 2);
    comp.set_cursor(solid_cursor(4, Color::rgb(255, 0, 0)), Point::new(30, 20));

    let mut display = MockDisplay::new(mode(64, 48));
    for step in 0..4i32 {
        comp.move_window(glass, Point::new(8 + step, 6));
        comp.present(&mut display)
            .expect("the mock present accepts");
        let totals = comp.frame_totals();
        assert_eq!(decoded(totals), Ok(totals), "step {step}");
    }
    let totals = comp.frame_totals();
    assert!(totals.blur_px > 0, "the frost was recomputed: {totals:?}");
    assert!(totals.blended_px > 0);
    assert!(totals.opaque_px > 0);
    assert_eq!(totals.frames, 4);
}

#[test]
fn a_mode_change_starts_a_fresh_epoch() {
    let mut comp = compositor();
    add(&mut comp, 0, 0, 10, 8, Color::rgb(9, 9, 9));
    first_frame(&mut comp);
    let before = comp.frame_totals();
    assert_eq!(before.screen_px, 64 * 48);

    assert!(comp.set_mode(mode(32, 24)), "the smaller mode allocates");
    comp.composite();
    let after = comp.frame_totals();
    assert_eq!(after.screen_px, 32 * 24);
    assert_eq!(
        after.frames, 1,
        "counts taken against another screen answer another question: {after:?}"
    );
    assert!(
        after.damaged_px <= after.screen_px,
        "the fresh epoch's damage is bounded by the fresh screen: {after:?}"
    );
    assert_eq!(decoded(after), Ok(after));
}

/// A wake that composed nothing is not a frame.
///
/// The desktop's run loop calls `present` on every wake, damaged or not, and
/// a reader watching these totals for change settles only while they hold
/// still. Counting the wakes themselves left the desktop's own frame
/// accounting climbing for ever on a idle screen, so that reader never
/// settled and paid a round trip on a cadence to republish it.
#[test]
fn an_undamaged_present_is_not_a_frame() {
    let mut comp = compositor();
    let mut display = MockDisplay::new(mode(64, 48));
    add(&mut comp, 0, 0, 10, 8, Color::rgb(9, 9, 9));
    assert!(comp.present(&mut display).is_ok());
    let settled = comp.frame_totals();
    assert_eq!(settled.frames, 1, "the damaged wake is the one frame");

    for wake in 0..8 {
        assert!(comp.present(&mut display).is_ok());
        assert_eq!(
            comp.frame_totals(),
            settled,
            "undamaged wake {wake} moved the totals"
        );
    }

    // And a wake that *does* change something still counts, so the guard
    // cannot be satisfied by never counting at all.
    assert!(comp.set_background(Color::rgb(1, 2, 3)));
    assert!(comp.present(&mut display).is_ok());
    assert_eq!(comp.frame_totals().frames, settled.frames + 1);
}
