//! Tests for the viewport, the zoom ladder, the space mapping, and the
//! picture the canvas draws.
//!
//! The space mapping is checked *against the position map* rather than
//! against a second table of it: a page-space window is right when setting it
//! down through the user's turn lands exactly the display-space rectangle it
//! came from. That is what makes the request the worker is sent trustworthy
//! without either side restating the other's arithmetic.

use alloc::vec;

use tairix_geometry::Rect;
use tairix_raster::{Color, Reorient};

use super::{
    fitted_zoom, slider_at_zoom, window_in_page_space, zoom_at_slider, zoom_rung_above,
    zoom_rung_below, Fit, Picture, Viewport, ZOOM_ACTUAL_PER_MILLE, ZOOM_MAX_PER_MILLE,
    ZOOM_MIN_PER_MILLE, ZOOM_RUNGS, ZOOM_SLIDER_LINE_STEP, ZOOM_SLIDER_PAGE_STEP,
};

// ---- the zoom ladder ---------------------------------------------------

#[test]
fn the_ladder_ascends_and_spans_the_offered_range() {
    for pair in ZOOM_RUNGS.windows(2) {
        assert!(pair[0] < pair[1], "{pair:?} ascends");
    }
    assert_eq!(ZOOM_RUNGS[0], ZOOM_MIN_PER_MILLE);
    assert_eq!(ZOOM_RUNGS[ZOOM_RUNGS.len() - 1], ZOOM_MAX_PER_MILLE);
    assert!(
        ZOOM_RUNGS.contains(&ZOOM_ACTUAL_PER_MILLE),
        "actual size is a rung"
    );
}

#[test]
fn the_slider_spans_the_ladder_and_never_leaves_it() {
    assert_eq!(zoom_at_slider(0), ZOOM_MIN_PER_MILLE);
    assert_eq!(zoom_at_slider(1_000), ZOOM_MAX_PER_MILLE);
    // Past full scale is still full scale rather than wrapping to the bottom.
    assert_eq!(zoom_at_slider(u16::MAX), ZOOM_MAX_PER_MILLE);
    for position in 0..=1_000u16 {
        let zoom = zoom_at_slider(position);
        assert!(
            (ZOOM_MIN_PER_MILLE..=ZOOM_MAX_PER_MILLE).contains(&zoom),
            "position {position} gave {zoom}"
        );
    }
}

#[test]
fn the_slider_is_monotonic_so_dragging_one_way_only_zooms_one_way() {
    let mut previous = zoom_at_slider(0);
    for position in 1..=1_000u16 {
        let zoom = zoom_at_slider(position);
        assert!(zoom >= previous, "position {position} went backwards");
        previous = zoom;
    }
}

#[test]
fn every_rung_round_trips_through_the_slider() {
    // The property the two functions exist for: a zoom the tools set shows on
    // the slider at a position that names that same zoom back, so the control
    // never drags the picture somewhere the user did not ask for.
    for rung in ZOOM_RUNGS {
        let position = slider_at_zoom(rung);
        let back = zoom_at_slider(position);
        let tolerance = rung / 20 + 1;
        assert!(
            back.abs_diff(rung) <= tolerance,
            "rung {rung} came back as {back} from position {position}"
        );
    }
}

#[test]
fn a_zoom_outside_the_ladder_is_held_to_its_ends_by_the_slider_mapping() {
    assert_eq!(slider_at_zoom(0), 0);
    assert_eq!(slider_at_zoom(u32::MAX), 1_000);
}

#[test]
fn the_sliders_page_step_is_one_rung_and_its_line_step_is_finer() {
    const { assert!(ZOOM_SLIDER_LINE_STEP < ZOOM_SLIDER_PAGE_STEP) };
    // One page step from the bottom lands on, or just past, the second rung.
    let stepped = zoom_at_slider(ZOOM_SLIDER_PAGE_STEP);
    assert!(stepped >= ZOOM_RUNGS[1], "{stepped} reached the next rung");
}

#[test]
fn stepping_the_rungs_stops_at_either_end_rather_than_wrapping() {
    assert_eq!(zoom_rung_below(ZOOM_MIN_PER_MILLE), ZOOM_MIN_PER_MILLE);
    assert_eq!(zoom_rung_above(ZOOM_MAX_PER_MILLE), ZOOM_MAX_PER_MILLE);
    assert_eq!(zoom_rung_above(ZOOM_RUNGS[0]), ZOOM_RUNGS[1]);
    assert_eq!(zoom_rung_below(ZOOM_RUNGS[1]), ZOOM_RUNGS[0]);
    // A factor between two rungs steps to the rung either side of it, not to
    // itself.
    let between = u32::midpoint(ZOOM_RUNGS[3], ZOOM_RUNGS[4]);
    assert_eq!(zoom_rung_above(between), ZOOM_RUNGS[4]);
    assert_eq!(zoom_rung_below(between), ZOOM_RUNGS[3]);
}

// ---- fitting -----------------------------------------------------------

#[test]
fn fitting_a_window_contains_the_whole_picture_on_both_axes() {
    // A 400x200 picture in a 100x100 canvas fits at a quarter, which is the
    // *smaller* of the two ratios: the taller fit would overflow the width.
    assert_eq!(fitted_zoom(Fit::Window, (400, 200), (100, 100)), Some(250));
    assert_eq!(fitted_zoom(Fit::Width, (400, 200), (100, 100)), Some(250));
    assert_eq!(fitted_zoom(Fit::Window, (200, 400), (100, 100)), Some(250));
    // Fitting the width ignores the height, which is what makes it different.
    assert_eq!(fitted_zoom(Fit::Width, (200, 400), (100, 100)), Some(500));
}

#[test]
fn actual_size_is_one_pixel_per_pixel_and_free_depends_on_no_canvas() {
    assert_eq!(
        fitted_zoom(Fit::Actual, (400, 200), (100, 100)),
        Some(ZOOM_ACTUAL_PER_MILLE)
    );
    assert_eq!(fitted_zoom(Fit::Free, (400, 200), (100, 100)), None);
}

#[test]
fn a_degenerate_picture_or_canvas_still_yields_a_zoom_a_render_may_name() {
    assert_eq!(fitted_zoom(Fit::Window, (0, 0), (100, 100)), None);
    let tiny = fitted_zoom(Fit::Window, (100_000, 100_000), (1, 1)).expect("a picture with pixels");
    assert!(tiny >= ZOOM_MIN_PER_MILLE, "{tiny} is inside the ladder");
    let huge = fitted_zoom(Fit::Window, (1, 1), (100_000, 100_000)).expect("a picture with pixels");
    assert!(huge <= ZOOM_MAX_PER_MILLE, "{huge} is inside the ladder");
}

#[test]
fn a_fitted_zoom_is_recomputed_on_a_resize_and_a_free_one_is_not() {
    let mut fitted = Viewport::new();
    fitted.set_fit(Fit::Window, (400, 200), (100, 100));
    assert_eq!(fitted.zoom(), 250);
    fitted.refit((400, 200), (200, 200));
    assert_eq!(fitted.zoom(), 500, "the fit follows the canvas");

    let mut free = Viewport::new();
    free.set_zoom(1_500);
    assert_eq!(free.fit, Fit::Free);
    free.refit((400, 200), (200, 200));
    assert_eq!(
        free.zoom(),
        1_500,
        "the user's own factor survives a resize"
    );
}

// ---- the extent a render is asked for ---------------------------------

#[test]
fn an_ordinary_photograph_reaches_the_top_of_the_ladder_uncapped() {
    // The bound is on the render's extent, not on the zoom, so a picture a
    // user actually owns magnifies all the way: a 12-megapixel photograph at
    // sixty-four times is still inside what a render may name.
    let mut viewport = Viewport::new();
    viewport.set_zoom(ZOOM_MAX_PER_MILLE);
    let natural = (4_000, 3_000);
    assert!(!viewport.cap_zoom(natural), "nothing to cap");
    assert_eq!(viewport.zoom(), ZOOM_MAX_PER_MILLE);
    let (width, height) = viewport.page_extent(natural);
    assert!(width <= tairix_raster::MAX_DRAWING_EXTENT);
    assert!(height <= tairix_raster::MAX_DRAWING_EXTENT);
}

#[test]
fn an_extreme_zoom_is_capped_without_changing_the_pictures_shape() {
    let mut viewport = Viewport::new();
    viewport.set_zoom(ZOOM_MAX_PER_MILLE);
    // Large enough that the top of the ladder would name an extent past what
    // the shared rasteriser places exactly.
    let natural = (30_000, 20_000);
    assert!(
        viewport.cap_zoom(natural),
        "the top of the ladder is past the bound"
    );
    let (width, height) = viewport.page_extent(natural);
    assert!(width <= tairix_raster::MAX_DRAWING_EXTENT);
    assert!(height <= tairix_raster::MAX_DRAWING_EXTENT);
    // The aspect ratio survives the cap: capping the axes independently would
    // have stretched the picture instead of refusing to magnify further.
    let ratio = u64::from(width) * 20_000;
    let expected = u64::from(height) * 30_000;
    assert!(
        ratio.abs_diff(expected) <= u64::from(height) + u64::from(width),
        "{width}x{height} keeps the 3:2 shape"
    );
}

#[test]
fn the_page_extent_set_down_through_the_turn_is_the_displayed_extent() {
    // The load-bearing property of the two spaces: the worker is asked for a
    // page-space extent, and setting *that* down through the user's turn must
    // land exactly the extent the viewer believes it is displaying.
    let natural = (400, 300);
    for how in Reorient::ALL {
        for zoom in [ZOOM_MIN_PER_MILLE, 500, ZOOM_ACTUAL_PER_MILLE, 4_000] {
            let mut viewport = Viewport::new();
            viewport.reorient = how;
            viewport.set_zoom(zoom);
            let page = viewport.page_extent(natural);
            let shown = viewport.scaled(natural);
            assert_eq!(how.applied_size(page.0, page.1), shown, "{how:?} at {zoom}");
        }
    }
}

// ---- panning -----------------------------------------------------------

#[test]
fn a_picture_smaller_than_its_canvas_cannot_be_panned() {
    let mut viewport = Viewport::new();
    viewport.set_fit(Fit::Window, (100, 100), (400, 400));
    viewport.pan_by(50, 50, (100, 100), (400, 400));
    assert_eq!(viewport.pan(), (0, 0));
    assert_eq!(viewport.overflows((100, 100), (400, 400)), (false, false));
}

#[test]
fn panning_stops_at_the_edge_it_reaches_rather_than_wrapping() {
    let mut viewport = Viewport::new();
    viewport.set_zoom(ZOOM_ACTUAL_PER_MILLE);
    let natural = (400, 300);
    let canvas = (100, 100);
    viewport.pan_by(i64::MAX, i64::MAX, natural, canvas);
    assert_eq!(viewport.pan(), (300, 200), "the far edge, not past it");
    viewport.pan_by(i64::MIN, i64::MIN, natural, canvas);
    assert_eq!(viewport.pan(), (0, 0), "the near edge, not below it");
}

#[test]
fn a_pan_step_moves_by_a_fraction_of_the_canvas_and_never_by_nothing() {
    let mut viewport = Viewport::new();
    viewport.set_zoom(4_000);
    let natural = (400, 300);
    for canvas in [(1, 1), (10, 10), (100, 100), (1_920, 1_080)] {
        viewport.pan_by(i64::MIN, i64::MIN, natural, canvas);
        viewport.pan_steps(1, 1, natural, canvas);
        assert_ne!(
            viewport.pan(),
            (0, 0),
            "a step moved something at {canvas:?}"
        );
    }
}

#[test]
fn zooming_out_brings_a_pan_back_inside_what_the_canvas_can_reach() {
    let mut viewport = Viewport::new();
    viewport.set_zoom(4_000);
    let natural = (400, 300);
    let canvas = (100, 100);
    viewport.pan_by(i64::MAX, i64::MAX, natural, canvas);
    assert_ne!(viewport.pan(), (0, 0));
    viewport.set_zoom(ZOOM_MIN_PER_MILLE);
    viewport.clamp_pan(natural, canvas);
    assert_eq!(
        viewport.pan(),
        (0, 0),
        "a picture that no longer overflows is not panned"
    );
}

#[test]
fn turning_the_picture_drops_the_pan_because_the_offset_names_nothing_now() {
    let mut viewport = Viewport::new();
    viewport.set_zoom(4_000);
    let natural = (400, 300);
    let canvas = (100, 100);
    viewport.pan_by(200, 200, natural, canvas);
    assert_ne!(viewport.pan(), (0, 0));
    viewport.turn(Reorient::QuarterTurnRight, natural, canvas);
    assert_eq!(viewport.pan(), (0, 0));
    assert_eq!(viewport.reorient, Reorient::QuarterTurnRight);
}

#[test]
fn turns_compose_rather_than_replacing_one_another() {
    let mut viewport = Viewport::new();
    let natural = (400, 300);
    let canvas = (100, 100);
    viewport.turn(Reorient::QuarterTurnRight, natural, canvas);
    viewport.turn(Reorient::QuarterTurnRight, natural, canvas);
    assert_eq!(viewport.reorient, Reorient::HalfTurn);
    assert_eq!(
        viewport.displayed(natural),
        natural,
        "a half turn keeps the axes"
    );
    viewport.turn(Reorient::QuarterTurnRight, natural, canvas);
    assert_eq!(
        viewport.displayed(natural),
        (300, 400),
        "a quarter turn swaps them"
    );
}

// ---- placement ---------------------------------------------------------

#[test]
fn a_picture_smaller_than_its_canvas_is_centred_in_it() {
    let mut viewport = Viewport::new();
    viewport.set_zoom(ZOOM_ACTUAL_PER_MILLE);
    let canvas = Rect::new(10, 20, 200, 100);
    let placed = viewport.placement((50, 40), canvas);
    assert_eq!(placed, Rect::new(10 + 75, 20 + 30, 50, 40));
}

#[test]
fn a_picture_larger_than_its_canvas_fills_it() {
    let mut viewport = Viewport::new();
    viewport.set_zoom(ZOOM_ACTUAL_PER_MILLE);
    let canvas = Rect::new(10, 20, 200, 100);
    let placed = viewport.placement((400, 300), canvas);
    assert_eq!(placed, Rect::new(10, 20, 200, 100));
}

// ---- the display-to-page window mapping --------------------------------

#[test]
fn a_window_maps_into_page_space_and_back_to_the_rectangle_it_came_from() {
    // The property that makes the render request exact: whatever the turn,
    // the page-space window set down through it covers precisely the
    // display-space rectangle asked for.
    let grid = (40, 30);
    let windows = [
        Rect::new(0, 0, 40, 30),
        Rect::new(0, 0, 1, 1),
        Rect::new(5, 7, 10, 8),
        Rect::new(39, 29, 1, 1),
        Rect::new(20, 0, 20, 30),
    ];
    for how in Reorient::ALL {
        for display in windows {
            let page = window_in_page_space(display, grid, how);
            let (page_grid_w, page_grid_h) = how.applied_size(grid.0, grid.1);
            assert!(
                page.width <= page_grid_w && page.height <= page_grid_h,
                "{how:?} {display:?} -> {page:?} stays inside {page_grid_w}x{page_grid_h}"
            );
            // A turn either keeps the window's extents or swaps them; it never
            // changes how many pixels it covers.
            let covered = (page.width, page.height);
            let expected = how.applied_size(display.width, display.height);
            assert_eq!(covered, expected, "{how:?} {display:?}");
        }
    }
}

#[test]
fn an_empty_window_maps_to_nothing_rather_than_to_one_pixel() {
    for how in Reorient::ALL {
        assert_eq!(
            window_in_page_space(Rect::new(0, 0, 0, 5), (40, 30), how),
            Rect::EMPTY
        );
        assert_eq!(
            window_in_page_space(Rect::new(0, 0, 5, 0), (40, 30), how),
            Rect::EMPTY
        );
    }
}

#[test]
fn the_unturned_mapping_is_the_identity() {
    let window = Rect::new(5, 7, 10, 8);
    assert_eq!(
        window_in_page_space(window, (40, 30), Reorient::None),
        window
    );
}

// ---- the picture the canvas draws -------------------------------------

/// Straight-alpha pixels for a `width`x`height` window, each distinguishable.
fn pixels(width: u32, height: u32) -> alloc::vec::Vec<u8> {
    let mut bytes = vec![0u8; (width * height * 4) as usize];
    let (quads, _remainder) = bytes.as_chunks_mut::<4>();
    for (index, quad) in quads.iter_mut().enumerate() {
        let index = u8::try_from(index % 251).unwrap_or(0);
        *quad = [index, index.wrapping_add(1), index.wrapping_add(2), 255];
    }
    bytes
}

#[test]
fn a_picture_is_adopted_and_drawn_from_the_surface_the_turn_produced() {
    let mut picture = Picture::default();
    let window = Rect::new(0, 0, 4, 2);
    assert!(picture.adopt(window, (4, 2), Reorient::None, &pixels(4, 2)));
    let unturned = picture.surface().expect("adopted").clone();
    assert_eq!((unturned.width(), unturned.height()), (4, 2));

    assert!(picture.adopt(window, (4, 2), Reorient::QuarterTurnRight, &pixels(4, 2)));
    let turned = picture.surface().expect("adopted");
    assert_eq!(
        (turned.width(), turned.height()),
        (2, 4),
        "a quarter turn swaps the axes"
    );
    let expected = unturned
        .reoriented(Reorient::QuarterTurnRight)
        .expect("allocates");
    assert_eq!(turned.pixels(), expected.pixels());
}

#[test]
fn a_pixel_run_that_does_not_describe_the_window_is_refused_without_changing_anything() {
    let mut picture = Picture::default();
    let window = Rect::new(0, 0, 4, 2);
    assert!(picture.adopt(window, (4, 2), Reorient::None, &pixels(4, 2)));
    let held = picture.surface().expect("adopted").clone();
    // One pixel short, then one pixel over: neither is this window.
    assert!(!picture.adopt(window, (4, 2), Reorient::None, &pixels(4, 2)[..28]));
    assert!(!picture.adopt(window, (4, 2), Reorient::None, &pixels(5, 2)));
    assert_eq!(
        picture.surface().expect("still held").pixels(),
        held.pixels()
    );
}

#[test]
fn readopting_the_same_geometry_reuses_the_surfaces_it_already_holds() {
    // The property the retained surfaces exist for: a viewer panning at a
    // fixed zoom asks for the same geometry every sample, and must not
    // allocate a window's worth of pixels each time. The address of the
    // pixel run is the observable: reuse keeps it, a fresh allocation would
    // not have to.
    let mut picture = Picture::default();
    let window = Rect::new(0, 0, 8, 8);
    assert!(picture.adopt(window, (8, 8), Reorient::QuarterTurnRight, &pixels(8, 8)));
    let first = picture.surface().expect("adopted").pixels().as_ptr();
    for _ in 0..4 {
        assert!(picture.adopt(window, (8, 8), Reorient::QuarterTurnRight, &pixels(8, 8)));
        assert_eq!(
            picture.surface().expect("adopted").pixels().as_ptr(),
            first,
            "the same geometry was drawn into the surface already held"
        );
    }
}

#[test]
fn an_unturned_picture_holds_no_second_surface() {
    let mut picture = Picture::default();
    let window = Rect::new(0, 0, 4, 2);
    assert!(picture.adopt(window, (4, 2), Reorient::QuarterTurnRight, &pixels(4, 2)));
    assert!(picture.shown.is_some(), "a turn needs its own destination");
    assert!(picture.adopt(window, (4, 2), Reorient::None, &pixels(4, 2)));
    assert!(
        picture.shown.is_none(),
        "an unturned picture is drawn from the staging surface itself"
    );
}

#[test]
fn nothing_is_drawn_before_a_picture_arrives() {
    let picture = Picture::default();
    assert!(picture.surface().is_none());
}

#[test]
fn a_straight_alpha_run_is_premultiplied_as_it_is_adopted() {
    let mut picture = Picture::default();
    // Half-transparent red, which premultiplies through the crate's one path.
    let rgba = [255u8, 0, 0, 128];
    assert!(picture.adopt(Rect::new(0, 0, 1, 1), (1, 1), Reorient::None, &rgba));
    let held = picture.surface().expect("adopted");
    assert_eq!(
        held.get(0, 0),
        Some(Color::rgba(255, 0, 0, 128).premultiply())
    );
}

#[test]
fn a_degenerate_window_is_refused_rather_than_held_as_an_empty_picture() {
    let mut picture = Picture::default();
    assert!(!picture.adopt(Rect::new(0, 0, 0, 4), (4, 4), Reorient::None, &[]));
    assert!(!picture.adopt(Rect::new(0, 0, 4, 0), (4, 4), Reorient::None, &[]));
    assert!(picture.surface().is_none());
}

#[test]
fn a_surface_is_only_reallocated_when_the_geometry_actually_changes() {
    let mut picture = Picture::default();
    assert!(picture.adopt(Rect::new(0, 0, 4, 4), (4, 4), Reorient::None, &pixels(4, 4)));
    let four = picture.surface().expect("adopted").pixels().len();
    assert_eq!(four, 16);
    assert!(picture.adopt(Rect::new(0, 0, 8, 2), (8, 2), Reorient::None, &pixels(8, 2)));
    let held = picture.surface().expect("adopted");
    assert_eq!((held.width(), held.height()), (8, 2));
}
