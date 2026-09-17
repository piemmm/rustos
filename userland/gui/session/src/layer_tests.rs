//! Layer-surface policy tests: the containment controls, the clamp, the
//! stacking, and the two coalesced feeds.

use super::{
    apply_participation, clamped_origin, fits_layer_bound, stack_at_depth, terrain_into, LayerFeed,
    LayerState, LayerSurface, LAYER_FITS_UNDER_TRUSTED_SURFACES,
};
use tairix_abi::window_ipc::{LayerDepth, TerrainPlate, DESKTOP_LAYER_MAX_SIDE_LOGICAL};
use tairix_geometry::Scale;
use tairix_wm::{Color, Compositor, Pixel, Point, Rect, Surface, WindowId};

use tairix_abi::driver::display::{DisplayFormat, DisplayMode};

const RED: Color = Color::rgb(255, 0, 0);

fn compositor(w: u32, h: u32) -> Compositor {
    let mode = DisplayMode {
        width_px: w,
        height_px: h,
        stride_bytes: w * 4,
        format: DisplayFormat::Rgba8888,
    };
    Compositor::new(
        mode,
        crate::tests::shell_for(tairix_taskbar::TaskbarConfig::bottom_bar(w, h))
            .session()
            .active_theme()
            .clone(),
        crate::tests::test_chrome_cache(),
        crate::tests::test_frost_cache(),
        crate::tests::test_pressure(),
    )
    .expect("a compositor for the test mode")
}

fn opaque(w: u32, h: u32) -> Surface {
    Surface::filled(w, h, RED.premultiply()).expect("surface allocates")
}

fn plate_buffer(len: usize) -> alloc::vec::Vec<TerrainPlate> {
    alloc::vec![
        TerrainPlate {
            x: 0,
            y: 0,
            width_px: 1,
            height_px: 1,
        };
        len
    ]
}

fn surface_at(wm: WindowId, depth: LayerDepth) -> LayerSurface {
    LayerSurface { ipc: 7, wm, depth }
}

/// A compositor-minted window id for the tests that exercise the feed
/// bookkeeping rather than any window's pixels. Ids are the compositor's to
/// mint, so one is taken from a throwaway rather than invented.
fn any_window_id() -> WindowId {
    compositor(64, 64).add_window(Point::new(0, 0), opaque(4, 4))
}

#[test]
fn the_bound_stays_below_every_trusted_surface() {
    // The proof is the `const` itself, which the build evaluates; naming it
    // here is what keeps a reader from thinking it is unreferenced.
    let () = LAYER_FITS_UNDER_TRUSTED_SURFACES;
}

#[test]
fn the_bound_is_measured_in_the_pixels_the_desktop_is_drawn_in() {
    let max = DESKTOP_LAYER_MAX_SIDE_LOGICAL;
    let one = Scale::from_percent(100).expect("the reference density");
    assert!(fits_layer_bound((max, max), one));
    assert!(!fits_layer_bound((max + 1, max), one));
    assert!(!fits_layer_bound((max, max + 1), one));

    // On a denser screen the same logical bound is more physical pixels, so
    // a surface cannot grow past it by asking at a higher scale either.
    let two = Scale::from_percent(200).expect("a doubled density");
    assert!(fits_layer_bound((max * 2, max * 2), two));
    assert!(!fits_layer_bound((max * 2 + 1, max * 2), two));
}

#[test]
fn an_off_screen_ask_is_clamped_rather_than_refused() {
    let work = Rect::new(0, 20, 800, 580);
    // An application is never told the screen's geometry, so asking for a
    // point outside it is an ordinary request the session resolves.
    assert_eq!(
        clamped_origin((-500, -500), (64, 64), work),
        Point::new(0, 20)
    );
    assert_eq!(
        clamped_origin((10_000, 10_000), (64, 64), work),
        Point::new(800 - 64, 600 - 64)
    );
    assert_eq!(
        clamped_origin((100, 100), (64, 64), work),
        Point::new(100, 100)
    );
}

#[test]
fn above_sits_under_the_bar_and_below_sits_under_every_window() {
    let mut c = compositor(200, 200);
    let app = c.add_window(Point::new(0, 0), opaque(200, 200));
    let bar = c.add_window(Point::new(0, 0), opaque(200, 20));
    let layer = c.add_window(Point::new(0, 40), opaque(50, 50));

    stack_at_depth(&mut c, layer, Some(bar), LayerDepth::Above);
    assert_eq!(c.window_at(Point::new(10, 10)), Some(bar));
    assert_eq!(c.window_at(Point::new(10, 50)), Some(layer));

    stack_at_depth(&mut c, layer, Some(bar), LayerDepth::Below);
    assert_eq!(c.window_at(Point::new(10, 50)), Some(app));
}

#[test]
fn above_without_a_bar_is_the_front_rather_than_a_guess() {
    let mut c = compositor(200, 200);
    let app = c.add_window(Point::new(0, 0), opaque(200, 200));
    let layer = c.add_window(Point::new(0, 40), opaque(50, 50));
    stack_at_depth(&mut c, layer, None, LayerDepth::Above);
    assert_eq!(c.window_at(Point::new(10, 50)), Some(layer));
    assert_eq!(c.window_at(Point::new(150, 150)), Some(app));
}

#[test]
fn terrain_reports_rectangles_back_to_front_and_never_the_asking_surface() {
    let mut c = compositor(200, 200);
    let back = c.add_window(Point::new(0, 0), opaque(30, 20));
    let front = c.add_window(Point::new(50, 60), opaque(40, 40));
    let layer = c.add_window(Point::new(5, 5), opaque(10, 10));

    let mut out = plate_buffer(8);
    let written = terrain_into(&c, layer, &mut out);
    assert_eq!(written, 2);
    assert_eq!(
        out[0],
        TerrainPlate {
            x: c.window(back).expect("tracked").bounds().left(),
            y: c.window(back).expect("tracked").bounds().top(),
            width_px: c.window(back).expect("tracked").bounds().width,
            height_px: c.window(back).expect("tracked").bounds().height,
        }
    );
    assert_eq!(out[1].x, c.window(front).expect("tracked").bounds().left());
}

#[test]
fn a_desktop_deeper_than_the_reply_keeps_the_frontmost_plates() {
    let mut c = compositor(400, 400);
    let mut ids = alloc::vec::Vec::new();
    for index in 0..5 {
        ids.push(c.add_window(Point::new(index * 10, index * 10), opaque(20, 20)));
    }
    let layer = c.add_window(Point::new(300, 300), opaque(10, 10));

    let mut out = plate_buffer(2);
    let written = terrain_into(&c, layer, &mut out);
    assert_eq!(written, 2);
    // The frontmost two, which are the ones a surface can actually meet.
    let fourth = c.window(ids[3]).expect("tracked").bounds();
    let fifth = c.window(ids[4]).expect("tracked").bounds();
    assert_eq!((out[0].x, out[0].y), (fourth.left(), fourth.top()));
    assert_eq!((out[1].x, out[1].y), (fifth.left(), fifth.top()));
}

#[test]
fn terrain_into_an_empty_buffer_writes_nothing() {
    let mut c = compositor(200, 200);
    c.add_window(Point::new(0, 0), opaque(20, 20));
    let layer = c.add_window(Point::new(50, 50), opaque(10, 10));
    assert_eq!(terrain_into(&c, layer, &mut []), 0);
}

#[test]
fn a_fresh_surface_is_owed_the_current_terrain_generation() {
    let mut state = LayerState::default();
    assert!(state.take_feeds().next().is_none(), "no surface, no feeds");

    state.opened(surface_at(any_window_id(), LayerDepth::Below));
    let feeds: alloc::vec::Vec<LayerFeed> = state.take_feeds().collect();
    assert_eq!(
        feeds,
        [LayerFeed::Terrain {
            ipc: 7,
            generation: 0
        }],
        "a surface that has seen no terrain is told at once"
    );
    assert!(
        state.take_feeds().next().is_none(),
        "and is not told the same generation twice"
    );
}

#[test]
fn a_burst_of_window_movement_costs_one_terrain_notice() {
    let mut state = LayerState::default();
    state.opened(surface_at(any_window_id(), LayerDepth::Below));
    let _ = state.take_feeds().count();

    for _ in 0..50 {
        state.terrain_changed();
    }
    let feeds: alloc::vec::Vec<LayerFeed> = state.take_feeds().collect();
    assert_eq!(
        feeds,
        [LayerFeed::Terrain {
            ipc: 7,
            generation: 50
        }],
        "the event carries a generation, so a drag costs one message"
    );
}

#[test]
fn pointer_samples_are_coalesced_to_one_a_frame_and_only_when_moved() {
    let mut state = LayerState::default();
    state.opened(surface_at(any_window_id(), LayerDepth::Below));
    let _ = state.take_feeds().count();

    for step in 0..20 {
        state.pointer_moved(Point::new(step, step));
    }
    let feeds: alloc::vec::Vec<LayerFeed> = state.take_feeds().collect();
    assert_eq!(
        feeds,
        [LayerFeed::Pointer {
            ipc: 7,
            x: 19,
            y: 19
        }],
        "a fast drag costs one message per frame, not one per sample"
    );

    // The same position again says nothing new, so an idle desktop is silent.
    state.pointer_moved(Point::new(19, 19));
    assert!(state.take_feeds().next().is_none());

    state.pointer_moved(Point::new(20, 19));
    assert_eq!(
        state.take_feeds().collect::<alloc::vec::Vec<_>>(),
        [LayerFeed::Pointer {
            ipc: 7,
            x: 20,
            y: 19
        }]
    );
}

#[test]
fn a_trusted_surface_stops_both_feeds_and_hides_the_surface() {
    let mut c = compositor(200, 200);
    let wm = c.add_window(Point::new(10, 10), opaque(20, 20));
    let mut state = LayerState::default();
    state.opened(surface_at(wm, LayerDepth::Above));
    let _ = state.take_feeds().count();

    assert!(state.set_suppressed(true, &mut c));
    assert!(state.is_suppressed());
    assert!(
        !c.window(wm).expect("tracked").is_visible(),
        "a pet must not be on screen over a password field"
    );

    state.terrain_changed();
    state.pointer_moved(Point::new(100, 100));
    assert!(
        state.take_feeds().next().is_none(),
        "nothing at all is delivered while a trusted surface is up"
    );

    // Lifting the suppression does not replay what happened behind it: the
    // terrain generation is owed (the desktop did change), but the pointer
    // positions taken over the prompt are gone, not queued.
    assert!(state.set_suppressed(false, &mut c));
    assert!(c.window(wm).expect("tracked").is_visible());
    let feeds: alloc::vec::Vec<LayerFeed> = state.take_feeds().collect();
    assert_eq!(
        feeds,
        [LayerFeed::Terrain {
            ipc: 7,
            generation: 1
        }],
        "no pointer sample from behind the prompt is replayed"
    );
}

#[test]
fn suppression_is_idempotent_and_survives_a_close() {
    let mut c = compositor(200, 200);
    let wm = c.add_window(Point::new(10, 10), opaque(20, 20));
    let mut state = LayerState::default();
    state.opened(surface_at(wm, LayerDepth::Above));

    assert!(state.set_suppressed(true, &mut c));
    assert!(!state.set_suppressed(true, &mut c), "already suppressed");

    // A surface retired while a prompt is up must not un-suppress the seat:
    // the next surface to open while the prompt is still there stays hidden.
    assert!(state.closed(7));
    assert!(state.is_suppressed());
    assert!(state.surface().is_none());
}

#[test]
fn closing_names_the_surface_and_leaves_another_alone() {
    let mut state = LayerState::default();
    state.opened(surface_at(any_window_id(), LayerDepth::Below));
    assert!(
        !state.closed(999),
        "an unrelated window closes nothing here"
    );
    assert!(state.surface().is_some());
    assert!(state.closed(7));
    assert!(state.surface().is_none());
    assert!(state.take_feeds().next().is_none());
}

#[test]
fn a_placed_depth_is_what_the_state_reports() {
    let mut state = LayerState::default();
    state.opened(surface_at(any_window_id(), LayerDepth::Below));
    state.placed(LayerDepth::Above);
    assert_eq!(
        state.surface().map(|surface| surface.depth),
        Some(LayerDepth::Above)
    );
}

#[test]
fn a_layer_surface_is_composited_from_the_moment_it_opens() {
    // The regression this guards: a window opened *unpresented* is hidden
    // until something maps it, and the only mapping path is the taskbar's —
    // which a companion is deliberately not on. It was therefore invisible
    // for its whole life, however faithfully it presented.
    let mut c = compositor(200, 200);
    let wm = c.add_window(Point::new(10, 10), opaque(20, 20));
    assert!(
        c.window(wm).expect("tracked").is_visible(),
        "a layer surface must be in the composite before its first present"
    );
}

#[test]
fn a_surface_with_nothing_drawn_in_it_catches_no_pointer() {
    // The other half of opening it live: it is in the stack at once, so it
    // must not swallow a click until the client has actually drawn.
    let mut c = compositor(200, 200);
    let under = c.add_window(Point::new(0, 0), opaque(200, 200));
    let blank = Surface::filled(40, 40, Pixel::TRANSPARENT).expect("a blank surface allocates");
    let layer = c.add_window(Point::new(20, 20), blank);
    apply_participation(&mut c, layer, None, LayerDepth::Above);
    assert_eq!(
        c.window_at(Point::new(30, 30)),
        Some(under),
        "an undrawn companion must not shadow what is behind it"
    );
}
