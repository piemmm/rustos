//! The target keeps the window's shape, never exceeds the cap, keeps every
//! zoom's step whole, and the bands it is cut into tile its rows exactly
//! once.

use super::*;
use crate::quality::Ladder;
use tairix_parallel::{Reversed, Serial};

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

    // A very tall one: the height binds instead, at the largest fraction
    // that brings it inside the cap.
    let tall = Viewport::new(1000, 4000, RenderScale::ONE).expect("a tall window");
    let (tw, th) = tall.render();
    assert!(th <= MAX_RENDER_HEIGHT && tw <= MAX_RENDER_WIDTH);
    let chosen = CAPS
        .iter()
        .position(|cap| *cap == tall.scale())
        .expect("a stated cap");
    let larger = CAPS[chosen - 1];
    assert!(
        larger.apply(4000) > MAX_RENDER_HEIGHT,
        "a larger fraction fitted"
    );
    assert_eq!(
        (tw, th),
        (tall.scale().apply(1000), tall.scale().apply(4000)),
        "the two axes were scaled by different fractions"
    );
}

#[test]
fn every_capped_step_is_whole() {
    for (w, h) in [(3840u32, 2160u32), (7680, 1080), (1000, 4000), (9000, 9000)] {
        for step in 0..=Ladder::MAX_STEP {
            let view = Viewport::new(w, h, Ladder::new(step).render_scale()).expect("a window");
            for zoom in [Zoom::NEAREST, Zoom::DEFAULT, Zoom::FURTHEST] {
                let base = i64::from(zoom.sub_units_per_pixel());
                let scale = view.scale();
                assert_eq!(
                    i64::from(view.step(zoom)) * i64::from(scale.numerator()),
                    base * i64::from(scale.denominator()),
                    "{w}x{h} at step {step} split a sub-unit at {zoom:?}"
                );
            }
        }
    }
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
fn bands_tile_every_row_and_none_is_longer_than_a_balanced_split() {
    for (w, h) in [(1280u32, 720u32), (17, 3), (1, 1), (640, 101), (64, 4001)] {
        let view = Viewport::new(w, h, RenderScale::ONE).expect("a real window");
        let (_, rendered) = view.render();
        for width in 1..=9usize {
            let runner = Reversed::new(width);
            let count = u32::try_from(view.band_count(&runner)).expect("a handful");
            let rows = view.band_rows(&runner);
            let bands = rendered.div_ceil(rows);
            assert!(
                bands <= count,
                "{bands} bands for a runner that asked for {count}"
            );
            assert!(
                rows * bands >= rendered,
                "{bands} bands of {rows} missed rows"
            );
            assert!(
                rows * (bands - 1) < rendered,
                "a band of {rows} rows held nothing"
            );
            assert_eq!(
                rows,
                rendered.div_ceil(count),
                "a band is longer than it need be"
            );
        }
    }
}

#[test]
fn a_serial_runner_asks_for_one_band() {
    let view = Viewport::new(1280, 720, RenderScale::ONE).expect("a real window");
    assert_eq!(view.band_count(&Serial), 1, "one thread wants one piece");
    assert_eq!(view.band_rows(&Serial), 720);
}

#[test]
fn a_tiny_target_is_never_split_below_one_row() {
    let view = Viewport::new(1, 1, RenderScale::ONE).expect("a one-pixel window");
    assert_eq!(view.band_count(&Reversed::new(16)), 1);
    assert_eq!(view.band_rows(&Reversed::new(16)), 1);
}

/// A window the software path draws whole is handed to the paint as it
/// stands, and no second buffer is made for it.
#[test]
fn a_native_window_is_drawn_in_its_own_pixels() {
    use tairix_raster::color::Color;
    use tairix_raster::surface::Surface;

    let colour = Color::rgb(10, 20, 30);
    let view = Viewport::new(64, 48, RenderScale::ONE).expect("a real window");
    let mut window = Surface::new(64, 48).expect("a window fits");
    let mut reduced = None;
    let drawn = view
        .draw_into(&mut window, &mut reduced, |target| {
            assert_eq!((target.width(), target.height()), (64, 48));
            target.fill(colour);
            Ok(7)
        })
        .expect("draws");
    assert_eq!(drawn, 7, "the paint's own answer comes back");
    assert!(reduced.is_none(), "a native frame needs no reduced target");
    assert!(window.pixels().iter().all(|p| *p == colour.premultiply()));
}

/// A window past the cap is drawn reduced and resampled up over the whole
/// window, and the reduced target is kept for the next frame at that extent.
#[test]
fn a_window_past_the_cap_is_drawn_reduced_and_resampled_up() {
    use tairix_raster::color::Color;
    use tairix_raster::surface::Surface;

    let colour = Color::rgb(200, 100, 50);
    let view = Viewport::new(3840, 2160, RenderScale::ONE).expect("a 4K window");
    let mut window = Surface::new(3840, 2160).expect("a window fits");
    let mut reduced = None;
    let paint = |target: &mut Surface| {
        assert_eq!((target.width(), target.height()), view.render());
        target.fill(colour);
        Ok(())
    };
    view.draw_into(&mut window, &mut reduced, paint)
        .expect("draws");
    assert!(window.pixels().iter().all(|p| *p == colour.premultiply()));
    let held = reduced.as_ref().map(|s| s.pixels().as_ptr());
    view.draw_into(&mut window, &mut reduced, paint)
        .expect("draws again");
    assert_eq!(
        reduced.as_ref().map(|s| s.pixels().as_ptr()),
        held,
        "an unchanged extent reuses the reduced target"
    );
}

/// A surface that is not the view's window is refused before anything is
/// drawn, and a paint's refusal comes back as it was.
#[test]
fn a_mismatched_window_or_a_refused_paint_draws_nothing() {
    use tairix_raster::surface::Surface;

    let view = Viewport::new(64, 48, RenderScale::ONE).expect("a real window");
    let mut wrong = Surface::new(48, 64).expect("fits");
    let mut reduced = None;
    let mut called = false;
    assert_eq!(
        view.draw_into(&mut wrong, &mut reduced, |_| {
            called = true;
            Ok(())
        }),
        Err(ClientError::Viewport)
    );
    assert!(!called, "the paint never ran over the wrong surface");
    let mut window = Surface::new(64, 48).expect("fits");
    assert_eq!(
        view.draw_into(&mut window, &mut reduced, |_| Err::<(), _>(
            ClientError::OutOfMemory
        )),
        Err(ClientError::OutOfMemory)
    );
}
