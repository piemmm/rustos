//! The target keeps the window's shape, never exceeds the cap, and the
//! bands it is cut into tile its rows exactly once.

use super::*;
use crate::quality::Ladder;
use tairix_parallel::Serial;

#[test]
fn a_window_within_the_cap_renders_at_its_own_size() {
    let view = Viewport::new(1280, 720, RenderScale::ONE).expect("a real window");
    assert_eq!(view.render(), (1280, 720));
    assert_eq!(view.window(), (1280, 720));
    assert!(!view.needs_resample());
    assert_eq!(view.render_pixels(), 1280 * 720);
}

#[test]
fn a_window_with_no_pixels_is_refused_rather_than_drawn_smaller() {
    assert_eq!(
        Viewport::new(0, 720, RenderScale::ONE),
        Err(ClientError::Viewport)
    );
    assert_eq!(
        Viewport::new(1280, 0, RenderScale::ONE),
        Err(ClientError::Viewport)
    );
}

#[test]
fn a_window_over_the_cap_is_rendered_at_the_cap_and_upscaled() {
    let view = Viewport::new(3840, 2160, RenderScale::ONE).expect("a 4K window");
    let (rw, rh) = view.render();
    assert!(rw <= MAX_RENDER_WIDTH && rh <= MAX_RENDER_HEIGHT);
    assert_eq!((rw, rh), (2560, 1440), "16:9 over the cap lands on the cap");
    assert!(view.needs_resample());
    assert_eq!(
        view.window(),
        (3840, 2160),
        "the window is still what it is"
    );
}

#[test]
fn capping_keeps_the_window_proportions() {
    // A very wide window: the width binds, and the height must come down
    // with it rather than being left at its own cap.
    let view = Viewport::new(7680, 1080, RenderScale::ONE).expect("an ultrawide window");
    let (rw, rh) = view.render();
    assert_eq!(rw, MAX_RENDER_WIDTH);
    assert!(rh <= MAX_RENDER_HEIGHT);
    let want = u64::from(1080u32) * u64::from(MAX_RENDER_WIDTH) / u64::from(7680u32);
    assert_eq!(u64::from(rh), want);

    // A very tall one: the height binds instead.
    let tall = Viewport::new(1000, 4000, RenderScale::ONE).expect("a tall window");
    let (tw, th) = tall.render();
    assert_eq!(th, MAX_RENDER_HEIGHT);
    assert!(tw <= MAX_RENDER_WIDTH);
}

#[test]
fn the_ladders_last_rung_shrinks_the_target_below_the_window() {
    let full = Viewport::new(1280, 720, Ladder::FULL.render_scale()).expect("full");
    let shed =
        Viewport::new(1280, 720, Ladder::new(Ladder::MAX_STEP).render_scale()).expect("fully shed");
    assert!(shed.render_pixels() < full.render_pixels());
    assert!(shed.needs_resample());
    assert_eq!(shed.window(), full.window(), "the window did not move");
    assert_eq!(shed.render(), (640, 360), "the coarsest rung is a half");
}

#[test]
fn bands_tile_every_row_exactly_once() {
    for (w, h) in [(1280u32, 720u32), (17, 3), (1, 1), (640, 101)] {
        let view = Viewport::new(w, h, RenderScale::ONE).expect("a real window");
        for count in 1..=9usize {
            let rows = view.band_rows(count);
            let mut next = 0usize;
            for index in 0..count {
                let (start, end) = rows.range(index).expect("index is inside the count");
                assert_eq!(
                    start, next,
                    "band {index} of {count} left a gap or overlapped"
                );
                assert!(end >= start);
                next = end;
            }
            assert_eq!(next, h as usize, "{count} bands did not cover {h} rows");
            assert_eq!(rows.range(count), None, "there is no band past the last");
        }
    }
}

#[test]
fn band_lengths_differ_by_at_most_one_row() {
    let view = Viewport::new(64, 101, RenderScale::ONE).expect("a real window");
    let rows = view.band_rows(8);
    let mut lengths = alloc::vec::Vec::new();
    for index in 0..rows.count() {
        let (start, end) = rows.range(index).expect("inside the count");
        lengths.push(end - start);
    }
    let (min, max) = (
        lengths.iter().copied().min().expect("at least one band"),
        lengths.iter().copied().max().expect("at least one band"),
    );
    assert!(max - min <= 1, "band lengths {lengths:?} are not balanced");
}

#[test]
fn a_serial_runner_asks_for_one_band() {
    let view = Viewport::new(1280, 720, RenderScale::ONE).expect("a real window");
    assert_eq!(view.band_count(&Serial), 1, "one thread wants one piece");
}

#[test]
fn a_tiny_target_is_never_split_below_one_band() {
    let view = Viewport::new(1, 1, RenderScale::ONE).expect("a one-pixel window");
    assert_eq!(view.band_count(&Serial), 1);
    assert_eq!(
        view.band_rows(0).count(),
        1,
        "a zero count is floored at one"
    );
    assert_eq!(view.band_rows(0).range(0), Some((0, 1)));
}
