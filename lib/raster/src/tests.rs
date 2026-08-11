//! Unit tests for the shared rasterisation primitives.

use tairix_reclaim::CachedBytes;

use crate::color::{Color, Pixel};
use crate::paint::Paint;
use crate::round::round_rect_coverage;
use crate::scan::FillRule;
use crate::surface::{Surface, SUBPIXEL};

const BLUE: Color = Color::rgb(0, 0, 255);
const RED: Color = Color::rgb(255, 0, 0);

// ---- colour / blending ----------------------------------------------

#[test]
fn premultiply_opaque_is_identity() {
    assert_eq!(
        RED.premultiply(),
        Pixel {
            r: 255,
            g: 0,
            b: 0,
            a: 255
        }
    );
}

#[test]
fn premultiply_transparent_clears_colour() {
    assert_eq!(
        Color::rgba(255, 255, 255, 0).premultiply(),
        Pixel::TRANSPARENT
    );
}

#[test]
fn over_opaque_source_replaces_destination() {
    let src = RED.premultiply();
    let dst = BLUE.premultiply();
    assert_eq!(src.over(dst), src);
}

#[test]
fn over_transparent_source_keeps_destination() {
    let dst = BLUE.premultiply();
    assert_eq!(Pixel::TRANSPARENT.over(dst), dst);
}

#[test]
fn over_half_alpha_blends_premultiplied() {
    let src = Color::rgba(255, 0, 0, 128).premultiply();
    let dst = BLUE.premultiply();
    assert_eq!(
        src.over(dst),
        Pixel {
            r: 128,
            g: 0,
            b: 127,
            a: 255
        }
    );
}

#[test]
fn over_opaque_shortcut_matches_the_general_blend() {
    // The `a == 255` shortcut must be exactly what `src + dst * (1 - src.a)`
    // computes, for every channel combination — not merely for a clean
    // primary colour.
    let general = |src: Pixel, dst: Pixel| {
        let inv = 255 - u32::from(src.a);
        let blend = |s: u8, d: u8| s.saturating_add(crate::color::div255(u32::from(d) * inv));
        Pixel {
            r: blend(src.r, dst.r),
            g: blend(src.g, dst.g),
            b: blend(src.b, dst.b),
            a: blend(src.a, dst.a),
        }
    };
    for channels in [0u8, 1, 17, 128, 254, 255] {
        for alpha in [0u8, 1, 128, 254, 255] {
            let src = Color::rgba(channels, 200, 3, alpha).premultiply();
            let dst = Color::rgba(9, channels, 240, 200).premultiply();
            assert_eq!(src.over(dst), general(src, dst), "{src:?} over {dst:?}");
        }
    }
}

#[test]
fn scale_alpha_extremes() {
    let p = RED.premultiply();
    assert_eq!(p.scale_alpha(255), p);
    assert_eq!(p.scale_alpha(0), Pixel::TRANSPARENT);
}

#[test]
fn scale_alpha_half() {
    assert_eq!(
        RED.premultiply().scale_alpha(128),
        Pixel {
            r: 128,
            g: 0,
            b: 0,
            a: 128
        }
    );
}

#[test]
fn unpremultiply_round_trips_opaque() {
    let c = Color::rgb(17, 200, 240);
    assert_eq!(c.premultiply().unpremultiply(), c);
}

#[test]
fn unpremultiply_transparent_is_transparent() {
    assert_eq!(Pixel::TRANSPARENT.unpremultiply(), Color::TRANSPARENT);
}

#[test]
fn unpremultiply_opaque_is_the_channels_verbatim() {
    // An opaque pixel is already straight-alpha, so the fast path returns its
    // channels with no per-channel divide — the value a whole-surface present
    // of an opaque window relies on.
    let opaque = Pixel {
        r: 3,
        g: 200,
        b: 255,
        a: 255,
    };
    assert_eq!(
        opaque.unpremultiply(),
        Color {
            r: 3,
            g: 200,
            b: 255,
            a: 255
        }
    );
}

#[test]
fn unpremultiply_partial_alpha_still_recovers_the_colour() {
    // The divide path is still exercised for a translucent pixel: half-alpha
    // red round-trips back to opaque-strength red at alpha 128.
    let c = Color::rgba(255, 0, 0, 128);
    assert_eq!(c.premultiply().unpremultiply(), c);
}

#[test]
fn desaturate_full_saturation_is_identity() {
    let p = Color::rgba(200, 40, 10, 255).premultiply();
    assert_eq!(p.desaturate(255), p);
}

#[test]
fn desaturate_none_greys_every_channel_and_keeps_alpha() {
    let grey = RED.premultiply().desaturate(0);
    // BT.601 luma of pure red: (77 * 255 + 128) >> 8.
    assert_eq!(grey.r, 77);
    assert_eq!(grey.r, grey.g);
    assert_eq!(grey.g, grey.b);
    assert_eq!(grey.a, 255);
}

#[test]
fn desaturate_leaves_a_grey_pixel_alone() {
    // The weights sum to exactly 256, so a grey is its own luma at every
    // saturation.
    let grey = Color::rgb(90, 90, 90).premultiply();
    for saturation in [0, 1, 128, 254, 255] {
        assert_eq!(grey.desaturate(saturation), grey, "saturation {saturation}");
    }
}

#[test]
fn desaturate_keeps_the_premultiplied_invariant() {
    // Every channel of a premultiplied pixel is <= a, and desaturating may
    // not break that: a translucent icon pixel would otherwise composite
    // brighter than it covers.
    for alpha in [0, 1, 17, 128, 254, 255] {
        for (r, g, b) in [
            (255, 0, 0),
            (0, 255, 0),
            (0, 0, 255),
            (255, 255, 0),
            (13, 200, 90),
        ] {
            let p = Color::rgba(r, g, b, alpha).premultiply();
            for saturation in [0, 90, 200, 255] {
                let out = p.desaturate(saturation);
                assert_eq!(out.a, p.a);
                assert!(
                    out.r <= out.a && out.g <= out.a && out.b <= out.a,
                    "{out:?} exceeds its own alpha (from {p:?} at {saturation})"
                );
            }
        }
    }
}

#[test]
fn desaturate_partway_lands_between_the_colour_and_its_grey() {
    let p = RED.premultiply();
    let part = p.desaturate(128);
    let grey = p.desaturate(0);
    assert!(part.r < p.r && part.r > grey.r);
    // The channels the colour did not use rise off zero toward the grey.
    assert!(part.g > 0 && part.g < grey.g);
}

#[test]
fn theme_rgba_converts_to_color_by_field_move() {
    let rgba = tairix_theme::Rgba::new(10, 20, 30, 40);
    assert_eq!(
        Color::from(rgba),
        Color {
            r: 10,
            g: 20,
            b: 30,
            a: 40
        }
    );
}

// ---- surface ---------------------------------------------------------

#[test]
fn new_surface_is_transparent() {
    let s = Surface::new(3, 2).expect("allocates");
    assert_eq!(s.width(), 3);
    assert_eq!(s.height(), 2);
    assert!(s.pixels().iter().all(|p| *p == Pixel::TRANSPARENT));
}

#[test]
fn surface_get_set_bounds() {
    let mut s = Surface::new(2, 2).expect("allocates");
    s.set(1, 1, RED.premultiply());
    assert_eq!(s.get(1, 1), Some(RED.premultiply()));
    assert_eq!(s.get(2, 0), None);
    s.set(9, 9, RED.premultiply()); // out of bounds: ignored
}

#[test]
fn fill_rect_is_clipped() {
    let mut s = Surface::new(4, 4).expect("allocates");
    s.fill_rect(2, 2, 10, 10, RED);
    assert_eq!(s.get(3, 3), Some(RED.premultiply()));
    assert_eq!(s.get(0, 0), Some(Pixel::TRANSPARENT));
}

#[test]
fn fill_sets_every_pixel() {
    let mut s = Surface::new(2, 2).expect("allocates");
    s.fill(BLUE);
    assert!(s.pixels().iter().all(|p| *p == BLUE.premultiply()));
}

// ---- rounded-rectangle fill ------------------------------------------

#[test]
fn fill_round_rect_zero_radius_matches_a_clipped_square_fill() {
    let mut s = Surface::new(4, 4).expect("allocates");
    s.fill_round_rect(2, 2, 10, 10, 0, RED);
    assert_eq!(s.get(3, 3), Some(RED.premultiply()));
    assert_eq!(s.get(0, 0), Some(Pixel::TRANSPARENT));
}

#[test]
fn fill_round_rect_zero_size_is_a_no_op() {
    let mut s = Surface::new(4, 4).expect("allocates");
    s.fill_round_rect(1, 1, 0, 3, 2, RED);
    s.fill_round_rect(1, 1, 3, 0, 2, RED);
    assert!(s.pixels().iter().all(|p| *p == Pixel::TRANSPARENT));
}

#[test]
fn fill_round_rect_corners_are_clear_and_composite_over_the_background() {
    let mut s = Surface::new(20, 20).expect("allocates");
    s.fill(BLUE);
    s.fill_round_rect(0, 0, 20, 20, 8, RED);
    // The extreme corner is outside the rounded arc, so the blue background
    // shows through untouched.
    assert_eq!(s.get(0, 0), Some(BLUE.premultiply()));
    // Deep inside the rounded rectangle is fully opaque red.
    assert_eq!(s.get(10, 10), Some(RED.premultiply()));
    // The corner arc itself is anti-aliased: neither the source nor the
    // background alone.
    let corner = s.get(2, 2).expect("in bounds");
    assert_ne!(corner, RED.premultiply());
    assert_ne!(corner, BLUE.premultiply());
}

/// The straightforward rounded-rectangle fill: evaluate every pixel's
/// coverage and composite it. [`Surface::fill_round_rect`] splits the same
/// shape into its four corner squares and the fully-covered remainder, which
/// must be a pure cost change; this loop is the yardstick that proves it and
/// lives only here, so production keeps one definition of the fill.
fn reference_round_rect(
    surface: &mut Surface,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    radius: u32,
    color: Color,
) {
    if w == 0 || h == 0 {
        return;
    }
    let source = color.premultiply();
    let x_end = x.saturating_add(w).min(surface.width());
    let y_end = y.saturating_add(h).min(surface.height());
    for row in y..y_end {
        for col in x..x_end {
            let coverage = round_rect_coverage(col - x, row - y, w, h, radius);
            if coverage == 0 {
                continue;
            }
            let Some(dst) = surface.get(col, row) else {
                continue;
            };
            surface.set(col, row, source.scale_alpha(coverage).over(dst));
        }
    }
}

/// A surface whose every pixel differs, so a fill that blends wrongly against
/// its destination cannot hide behind a uniform background.
fn patterned_surface(width: u32, height: u32) -> Surface {
    let mut surface = Surface::new(width, height).expect("allocates");
    for y in 0..height {
        for x in 0..width {
            let channel = |factor: u32| u8::try_from((x * factor + y * 7) % 256).unwrap_or(0);
            let color = Color::rgba(channel(3), channel(11), channel(29), channel(53));
            surface.set(x, y, color.premultiply());
        }
    }
    surface
}

#[test]
fn fill_round_rect_matches_the_per_pixel_reference() {
    // Degenerate and ordinary geometries together: a radius of zero, a radius
    // larger than half the shorter side, rectangles clipped on the right and
    // bottom, one-pixel sides, and colours from fully transparent to opaque.
    for &(surface_w, surface_h) in &[(1u32, 1u32), (5, 4), (20, 20), (23, 9)] {
        for &(x, y, w, h) in &[
            (0u32, 0u32, 1u32, 1u32),
            (0, 0, surface_w, surface_h),
            (1, 1, surface_w, surface_h),
            (2, 1, 7, 5),
            (surface_w - 1, surface_h - 1, 9, 9),
            (0, 0, 3, 40),
            (0, 0, 40, 3),
        ] {
            for radius in [0u32, 1, 2, 3, 8, 100] {
                for alpha in [0u8, 1, 64, 128, 254, 255] {
                    let color = Color::rgba(200, 30, 60, alpha);
                    let mut actual = patterned_surface(surface_w, surface_h);
                    let mut expected = actual.clone();
                    actual.fill_round_rect(x, y, w, h, radius, color);
                    reference_round_rect(&mut expected, x, y, w, h, radius, color);
                    for (index, (got, want)) in
                        actual.pixels().iter().zip(expected.pixels()).enumerate()
                    {
                        assert_eq!(
                            got, want,
                            "pixel {index} of {surface_w}x{surface_h} differs for \
                             rect ({x},{y},{w},{h}) radius {radius} alpha {alpha}"
                        );
                    }
                }
            }
        }
    }
}

// ---- from_rgba8 --------------------------------------------------------

#[test]
fn from_rgba8_rejects_a_length_mismatch() {
    // One pixel short of the 2x2x4 = 16 bytes a 2x2 image needs.
    let rgba = [0u8; 15];
    assert_eq!(Surface::from_rgba8(2, 2, &rgba), None);
}

#[test]
fn from_rgba8_premultiplies_each_pixel() {
    // Straight-alpha half-red at alpha 128 premultiplies to (128, 0, 0, 128)
    // through the crate's one `Color::premultiply` path.
    let rgba = [255u8, 0, 0, 128];
    let s = Surface::from_rgba8(1, 1, &rgba).expect("length matches");
    assert_eq!(s.width(), 1);
    assert_eq!(s.height(), 1);
    assert_eq!(s.get(0, 0), Some(Color::rgba(255, 0, 0, 128).premultiply()));
}

#[test]
fn from_rgba8_reproduces_several_known_pixels() {
    let rgba = [
        255, 0, 0, 255, // opaque red
        0, 255, 0, 0, // fully transparent green (premultiplies to black)
        0, 0, 255, 255, // opaque blue
        10, 20, 30, 40, // an arbitrary translucent pixel
    ];
    let s = Surface::from_rgba8(2, 2, &rgba).expect("length matches");
    assert_eq!(s.get(0, 0), Some(RED.premultiply()));
    assert_eq!(s.get(1, 0), Some(Pixel::TRANSPARENT));
    assert_eq!(s.get(0, 1), Some(BLUE.premultiply()));
    assert_eq!(s.get(1, 1), Some(Color::rgba(10, 20, 30, 40).premultiply()));
}

#[test]
fn from_rgba8_zero_size_matches_surface_new() {
    assert_eq!(Surface::from_rgba8(0, 0, &[]), Surface::new(0, 0));
}

// ---- anti-aliased polygon fill --------------------------------------

#[test]
fn fill_polygon_covering_whole_grid_is_opaque() {
    let mut s = Surface::new(4, 4).expect("allocates");
    let square = [(0, 0), (4, 0), (4, 4), (0, 4)];
    s.fill_polygon(&square, 4, RED);
    assert!(s.pixels().iter().all(|p| *p == RED.premultiply()));
}

#[test]
fn fill_polygon_degenerate_ring_is_a_no_op() {
    let mut s = Surface::new(4, 4).expect("allocates");
    s.fill_polygon(&[(0, 0), (4, 4)], 4, RED);
    assert!(s.pixels().iter().all(|p| *p == Pixel::TRANSPARENT));
}

#[test]
fn fill_polygon_zero_design_does_not_panic() {
    let mut s = Surface::new(4, 4).expect("allocates");
    // A zero design grid is treated as 1 rather than dividing by zero.
    s.fill_polygon(&[(0, 0), (1, 0), (1, 1), (0, 1)], 0, RED);
    assert!(s.pixels().iter().all(|p| *p == RED.premultiply()));
}

#[test]
fn fill_polygon_triangle_is_anti_aliased() {
    let mut s = Surface::new(4, 4).expect("allocates");
    // Upper-left half: the diagonal edge crosses interior pixels.
    s.fill_polygon(&[(0, 0), (4, 0), (0, 4)], 4, RED);

    // A pixel straddling the diagonal has fractional coverage.
    let edge = s.get(1, 2).expect("in bounds");
    assert!(
        edge.a > 0 && edge.a < 255,
        "expected partial coverage: {edge:?}"
    );

    // The far corner is wholly outside the triangle.
    assert_eq!(s.get(3, 3), Some(Pixel::TRANSPARENT));

    // The opposite corner is wholly inside and opaque.
    assert_eq!(s.get(0, 0), Some(RED.premultiply()));
}

/// The bounding-box restriction is a pure optimisation: it must leave every
/// pixel outside a small shape's box exactly as a full-canvas scan would
/// (untouched), while still painting the shape itself correctly, so only the
/// scan cost changes, never a pixel.
#[test]
fn fill_polygon_small_shape_only_touches_its_own_bounding_box() {
    let mut s = Surface::new(40, 40).expect("allocates");
    // A right triangle occupying only the design range [10, 20) of a
    // 40-unit design grid: a small corner of the 40x40 canvas.
    let triangle = [(10, 10), (20, 10), (10, 20)];
    s.fill_polygon(&triangle, 40, RED);

    // Every corner of the canvas, far from the triangle, is untouched.
    assert_eq!(s.get(0, 0), Some(Pixel::TRANSPARENT));
    assert_eq!(s.get(39, 0), Some(Pixel::TRANSPARENT));
    assert_eq!(s.get(0, 39), Some(Pixel::TRANSPARENT));
    assert_eq!(s.get(39, 39), Some(Pixel::TRANSPARENT));

    // The right-angle corner of the triangle is deep inside it.
    assert_eq!(s.get(10, 10), Some(RED.premultiply()));

    // The hypotenuse crosses this pixel with partial coverage.
    let edge = s.get(15, 14).expect("in bounds");
    assert!(
        edge.a > 0 && edge.a < 255,
        "expected partial coverage near the hypotenuse: {edge:?}"
    );
}

// ---- device-space (grid-fitted) polygon fill -------------------------

#[test]
fn a_subpixel_polygon_on_whole_pixels_has_no_fringe() {
    // The point of the device-space entry: a caller that has rounded its shape
    // to whole pixels gets exactly those pixels and nothing else, with no
    // anti-aliased fringe to soften a small mark into a grey smear.
    let mut s = Surface::new(8, 8).expect("allocates");
    let rect = [
        (2 * SUBPIXEL, 3 * SUBPIXEL),
        (6 * SUBPIXEL, 3 * SUBPIXEL),
        (6 * SUBPIXEL, 5 * SUBPIXEL),
        (2 * SUBPIXEL, 5 * SUBPIXEL),
    ];
    s.fill_polygon_subpixel(&rect, RED);
    for y in 0..8 {
        for x in 0..8 {
            let inside = (2..6).contains(&x) && (3..5).contains(&y);
            let want = if inside {
                RED.premultiply()
            } else {
                Pixel::TRANSPARENT
            };
            assert_eq!(s.get(x, y), Some(want), "pixel ({x}, {y})");
        }
    }
}

#[test]
fn a_subpixel_polygon_off_the_pixel_grid_is_anti_aliased() {
    // The same bar shifted half a pixel across covers two columns at partial
    // alpha instead. That is what grid fitting exists to avoid — and still what
    // a genuinely fractional shape has to produce.
    let mut s = Surface::new(4, 4).expect("allocates");
    let half = SUBPIXEL / 2;
    let bar = [
        (SUBPIXEL + half, 0),
        (2 * SUBPIXEL + half, 0),
        (2 * SUBPIXEL + half, 4 * SUBPIXEL),
        (SUBPIXEL + half, 4 * SUBPIXEL),
    ];
    s.fill_polygon_subpixel(&bar, RED);
    for x in [1, 2] {
        let p = s.get(x, 1).expect("in bounds");
        assert!(
            p.a > 0 && p.a < 255,
            "expected partial coverage at {x}: {p:?}"
        );
    }
}

#[test]
fn a_subpixel_polygon_is_placed_not_stretched() {
    // Unlike `fill_polygon`, which maps a design grid across the whole surface,
    // the device-space entry draws where its coordinates say — so a small mark
    // needs no square scratch surface and blit to be positioned.
    let mut s = Surface::new(16, 16).expect("allocates");
    let square = [
        (10 * SUBPIXEL, 10 * SUBPIXEL),
        (12 * SUBPIXEL, 10 * SUBPIXEL),
        (12 * SUBPIXEL, 12 * SUBPIXEL),
        (10 * SUBPIXEL, 12 * SUBPIXEL),
    ];
    s.fill_polygon_subpixel(&square, RED);
    assert_eq!(s.get(10, 10), Some(RED.premultiply()));
    assert_eq!(s.get(0, 0), Some(Pixel::TRANSPARENT));
    assert_eq!(s.get(12, 12), Some(Pixel::TRANSPARENT));
}

#[test]
fn a_degenerate_subpixel_ring_is_a_no_op() {
    let mut s = Surface::new(4, 4).expect("allocates");
    s.fill_polygon_subpixel(&[(0, 0), (4 * SUBPIXEL, 4 * SUBPIXEL)], RED);
    assert!(s.pixels().iter().all(|p| *p == Pixel::TRANSPARENT));
}

// ---- stroked polyline ------------------------------------------------

/// Every slope draws, and draws in bounded time.
///
/// The regression: the segment length came from a hand-rolled Newton
/// iteration that stopped only when two successive estimates agreed, and for
/// a squared length one below a perfect square (8 here, from a (2, 2) step)
/// the estimates cycle between two values and never agree. A graph trace or a
/// furniture diagonal that happened to step that way wedged its process in a
/// loop that issues no syscall — a pegged core, and a window that never
/// answers again.
#[test]
fn a_stroke_of_any_slope_draws_and_terminates() {
    const CENTRE: i32 = 4 * SUBPIXEL;
    for dx in -24..=24 {
        for dy in -24..=24 {
            let mut s = Surface::new(8, 8).expect("allocates");
            s.stroke_polyline(
                &[(CENTRE, CENTRE), (CENTRE + dx, CENTRE + dy)],
                SUBPIXEL,
                RED,
            );
            let painted = s.pixels().iter().any(|p| *p != Pixel::TRANSPARENT);
            // A step of a whole pixel or more always leaves a mark; a shorter
            // one may fall between the coverage samples, and coincident
            // points are no segment at all.
            if dx.abs() >= SUBPIXEL || dy.abs() >= SUBPIXEL {
                assert!(painted, "step ({dx}, {dy}) drew nothing");
            } else if (dx, dy) == (0, 0) {
                assert!(!painted, "coincident points are not a segment");
            }
        }
    }
}

/// A segment far longer than a screen keeps its stated weight.
///
/// The regression: the squared length was accumulated in `i32` and saturated,
/// so a long segment measured shorter than it is and its perpendicular offset
/// — the half weight divided by that length — came out proportionally too
/// large. A one-pixel line then painted as a band tens of pixels wide,
/// swallowing the surface.
#[test]
fn a_stroke_longer_than_the_surface_keeps_its_weight() {
    let mut s = Surface::new(64, 64).expect("allocates");
    let far = 2_000_000;
    s.stroke_polyline(&[(-far, -far), (far, far)], SUBPIXEL, RED);

    let on_diagonal = s.get(10, 10).expect("in bounds");
    assert!(on_diagonal.a > 0, "the trace itself must be drawn");
    for (x, y) in [(0, 63), (63, 0), (0, 40), (40, 0)] {
        assert_eq!(
            s.get(x, y),
            Some(Pixel::TRANSPARENT),
            "a hairline must not reach ({x}, {y})"
        );
    }
}

#[test]
fn a_stroke_needs_two_points_and_a_positive_weight() {
    let mut s = Surface::new(4, 4).expect("allocates");
    s.stroke_polyline(&[(0, 0)], SUBPIXEL, RED);
    s.stroke_polyline(&[(0, 0), (4 * SUBPIXEL, 0)], 0, RED);
    s.stroke_polyline(&[(0, 0), (4 * SUBPIXEL, 0)], -SUBPIXEL, RED);
    assert!(s.pixels().iter().all(|p| *p == Pixel::TRANSPARENT));
}

// ---- blit ------------------------------------------------------------

#[test]
fn blit_composites_only_opaque_source_pixels() {
    let mut dst = Surface::new(4, 4).expect("allocates");
    dst.fill(BLUE);
    let mut src = Surface::new(2, 2).expect("allocates");
    src.set(0, 0, RED.premultiply()); // one opaque pixel; the rest transparent
    dst.blit(1, 1, &src);
    assert_eq!(dst.get(1, 1), Some(RED.premultiply()));
    // A transparent source pixel left the blue background untouched.
    assert_eq!(dst.get(2, 2), Some(BLUE.premultiply()));
    // Outside the blit footprint is also untouched.
    assert_eq!(dst.get(0, 0), Some(BLUE.premultiply()));
}

#[test]
fn blit_desaturated_greys_the_source_without_touching_it() {
    let mut dst = Surface::new(2, 2).expect("allocates");
    let mut src = Surface::new(2, 2).expect("allocates");
    src.fill(RED);
    dst.blit_desaturated(0, 0, &src, 0);
    assert_eq!(dst.get(0, 0), Some(RED.premultiply().desaturate(0)));
    // The sprite itself is unchanged, so one cached copy serves every state.
    assert_eq!(src.get(0, 0), Some(RED.premultiply()));
}

#[test]
fn a_fully_saturated_blit_is_the_plain_blit() {
    let mut src = Surface::new(3, 3).expect("allocates");
    src.fill(RED);
    src.set(1, 1, Color::rgba(0, 200, 40, 128).premultiply());
    let mut plain = Surface::new(3, 3).expect("allocates");
    plain.fill(BLUE);
    let mut mapped = Surface::new(3, 3).expect("allocates");
    mapped.fill(BLUE);
    plain.blit(0, 0, &src);
    mapped.blit_desaturated(0, 0, &src, 255);
    assert_eq!(plain.pixels(), mapped.pixels());
}

#[test]
fn a_desaturated_blit_clips_exactly_as_a_plain_one_does() {
    let mut src = Surface::new(4, 4).expect("allocates");
    src.fill(RED);
    let mut dst = Surface::new(2, 2).expect("allocates");
    dst.blit_desaturated(-1, -1, &src, 0);
    let grey = RED.premultiply().desaturate(0);
    assert!(dst.pixels().iter().all(|p| *p == grey));
}

#[test]
fn set_round_rect_lays_a_translucent_ground_a_fill_could_not() {
    // The point of the primitive: a half-opaque colour composited over an
    // opaque plate comes back opaque, so a surface that must be see-through
    // has to *replace* what it covers.
    let ground = Color::rgba(20, 26, 30, 128);
    let mut laid = Surface::new(8, 8).expect("allocates");
    laid.fill(BLUE);
    laid.set_round_rect(0, 0, 8, 8, 0, ground);
    assert_eq!(laid.get(4, 4), Some(ground.premultiply()));

    let mut filled = Surface::new(8, 8).expect("allocates");
    filled.fill(BLUE);
    filled.fill_round_rect(0, 0, 8, 8, 0, ground);
    assert_eq!(filled.get(4, 4).map(|p| p.a), Some(255));
}

/// Every control background is *laid down* rather than composited, so that a
/// translucent one keeps the alpha its theme authored. This is what makes
/// that safe for the opaque ones: an opaque colour covers what is under it
/// either way. Wherever the shape fully covers a pixel the two are the same
/// byte; on an arc pixel both compute the same blend, laying it down with a
/// single rounding where compositing rounds the source and the destination
/// separately, so it can land one level nearer the exact value and never
/// further.
#[test]
fn laying_an_opaque_colour_down_matches_compositing_it_to_within_a_rounding() {
    let under = Color::rgba(254, 3, 200, 255);
    let over = Color::rgba(1, 251, 40, 255);
    for radius in [0, 1, 5, 8] {
        let mut laid = Surface::new(16, 16).expect("allocates");
        laid.fill(under);
        laid.set_round_rect(0, 0, 16, 16, radius, over);

        let mut composited = Surface::new(16, 16).expect("allocates");
        composited.fill(under);
        composited.fill_round_rect(0, 0, 16, 16, radius, over);

        let exact = |x: u32, y: u32| {
            let inset = radius.saturating_add(1);
            (inset..16 - inset).contains(&x) || (inset..16 - inset).contains(&y)
        };
        for y in 0..16 {
            for x in 0..16 {
                let (a, b) = (
                    laid.get(x, y).expect("in bounds"),
                    composited.get(x, y).expect("in bounds"),
                );
                let apart = [(a.r, b.r), (a.g, b.g), (a.b, b.b), (a.a, b.a)]
                    .into_iter()
                    .map(|(l, r)| u32::from(l.abs_diff(r)))
                    .max()
                    .unwrap_or(0);
                if exact(x, y) {
                    assert_eq!(a, b, "r{radius} ({x},{y}) is fully covered");
                } else {
                    assert!(apart <= 1, "r{radius} ({x},{y}): {a:?} vs {b:?}");
                }
            }
        }
    }
}

#[test]
fn set_round_rect_leaves_everything_outside_its_shape_alone() {
    let mut surface = Surface::new(8, 8).expect("allocates");
    surface.fill(RED);
    let red = RED.premultiply();
    surface.set_round_rect(2, 2, 4, 4, 0, Color::rgba(0, 0, 0, 0));
    assert_eq!(surface.get(1, 4), Some(red), "the column beside it");
    assert_eq!(surface.get(4, 1), Some(red), "the row above it");
    assert_eq!(
        surface.get(3, 3),
        Some(Pixel::TRANSPARENT),
        "and inside it is exactly what was laid"
    );
}

#[test]
fn set_round_rect_mixes_an_arc_pixel_toward_the_ground() {
    // A pixel the arc partly covers keeps that fraction of what was under
    // it, so the rim a floating plate is laid over still shows through its
    // own arc instead of being punched out to a translucent notch.
    let mut surface = Surface::new(16, 16).expect("allocates");
    surface.fill(RED);
    let red = RED.premultiply();
    let ground = Color::rgba(0, 0, 40, 128);
    let laid = ground.premultiply();
    surface.set_round_rect(0, 0, 16, 16, 6, ground);

    let partial = (0..6u32)
        .flat_map(|x| (0..6u32).map(move |y| (x, y)))
        .filter_map(|(x, y)| surface.get(x, y))
        .find(|p| p.a > laid.a && p.a < red.a);
    assert!(
        partial.is_some(),
        "no pixel on the arc kept part of what was under it"
    );
    assert_eq!(
        surface.get(0, 0),
        Some(red),
        "and the corner the arc misses entirely is untouched"
    );
}

#[test]
fn blit_clips_negative_origin_and_overflow() {
    let mut dst = Surface::new(2, 2).expect("allocates");
    let mut src = Surface::new(4, 4).expect("allocates");
    src.fill(RED);
    // Top-left corner placed off-surface: only the overlapping part lands.
    dst.blit(-1, -1, &src);
    assert!(dst.pixels().iter().all(|p| *p == RED.premultiply()));
}

#[test]
fn fill_polygon_composites_over_existing_pixels() {
    let mut s = Surface::new(2, 2).expect("allocates");
    s.fill(BLUE);
    // A half-transparent red square over an opaque blue background.
    let square = [(0, 0), (2, 0), (2, 2), (0, 2)];
    s.fill_polygon(&square, 2, Color::rgba(255, 0, 0, 128));
    let blended = Color::rgba(255, 0, 0, 128)
        .premultiply()
        .over(BLUE.premultiply());
    assert!(s.pixels().iter().all(|p| *p == blended));
}

// ---- clip window -----------------------------------------------------
//
// Every write path is confined by the clip window, which is what lets a view
// bound what it draws to the area it owns without any drawing routine trimming
// its own geometry. Each primitive is checked separately: one honouring the
// window while another forgets it would leak paint onto a neighbour's pixels.

#[test]
fn clip_confines_a_rect_fill() {
    let mut s = Surface::new(4, 4).expect("allocates");
    s.with_clip(1, 1, 2, 2, |s| s.fill_rect(0, 0, 4, 4, RED));
    for y in 0..4 {
        for x in 0..4 {
            let inside = (1..3).contains(&x) && (1..3).contains(&y);
            let expected = if inside {
                RED.premultiply()
            } else {
                Pixel::TRANSPARENT
            };
            assert_eq!(s.get(x, y), Some(expected), "at ({x}, {y})");
        }
    }
}

#[test]
fn clip_confines_a_whole_surface_fill() {
    let mut s = Surface::new(3, 3).expect("allocates");
    s.with_clip(0, 0, 3, 1, |s| s.fill(RED));
    assert_eq!(s.get(2, 0), Some(RED.premultiply()));
    assert_eq!(s.get(0, 1), Some(Pixel::TRANSPARENT));
}

#[test]
fn clip_confines_a_rounded_fill_without_re_rounding_it() {
    // The full shape's own corner coverage, for comparison.
    let mut whole = Surface::new(8, 8).expect("allocates");
    whole.fill_round_rect(0, 0, 8, 8, 3, RED);

    // The same shape drawn through a window that cuts its left half: the
    // surviving pixels must be identical to the whole shape's, so a clipped
    // tile keeps the corner arcs of the tile rather than of the sliver.
    let mut cut = Surface::new(8, 8).expect("allocates");
    cut.with_clip(4, 0, 4, 8, |s| s.fill_round_rect(0, 0, 8, 8, 3, RED));
    for y in 0..8 {
        for x in 0..8 {
            let expected = if x < 4 {
                Pixel::TRANSPARENT
            } else {
                whole.get(x, y).expect("in bounds")
            };
            assert_eq!(cut.get(x, y), Some(expected), "at ({x}, {y})");
        }
    }
}

#[test]
fn clip_confines_a_polygon_fill() {
    let mut s = Surface::new(4, 4).expect("allocates");
    let square = [(0, 0), (4, 0), (4, 4), (0, 4)];
    s.with_clip(2, 2, 2, 2, |s| s.fill_polygon(&square, 4, RED));
    assert_eq!(s.get(3, 3), Some(RED.premultiply()));
    assert_eq!(s.get(1, 1), Some(Pixel::TRANSPARENT));
}

/// A sprite is clipped on every side, and the surviving pixels keep their
/// source alignment — a blit that skipped clipped columns in the destination
/// but not in the source would smear the sprite sideways.
#[test]
fn clip_confines_a_blit_and_keeps_the_source_aligned() {
    let mut src = Surface::new(4, 1).expect("allocates");
    for x in 0..4 {
        // A distinct alpha per column, so a misalignment is visible.
        src.set(
            x,
            0,
            Color::rgba(255, 0, 0, 60 + 40 * u8::try_from(x).expect("small")).premultiply(),
        );
    }
    let mut dst = Surface::new(4, 1).expect("allocates");
    dst.with_clip(1, 0, 2, 1, |s| s.blit(0, 0, &src));
    assert_eq!(dst.get(0, 0), Some(Pixel::TRANSPARENT));
    assert_eq!(dst.get(1, 0), src.get(1, 0));
    assert_eq!(dst.get(2, 0), src.get(2, 0));
    assert_eq!(dst.get(3, 0), Some(Pixel::TRANSPARENT));
}

#[test]
fn clip_confines_a_single_pixel_write() {
    let mut s = Surface::new(2, 2).expect("allocates");
    s.with_clip(0, 0, 1, 1, |s| {
        s.set(0, 0, RED.premultiply());
        s.set(1, 1, RED.premultiply());
    });
    assert_eq!(s.get(0, 0), Some(RED.premultiply()));
    assert_eq!(s.get(1, 1), Some(Pixel::TRANSPARENT));
}

/// A row span is the one place a write is confined, so it reports the column
/// it really starts at: a caller pairing it with its own mask (the glyph
/// blitter) advances that mask by the difference.
#[test]
fn row_span_reports_the_column_the_clip_left() {
    let mut s = Surface::new(8, 2).expect("allocates");
    s.with_clip(3, 0, 2, 2, |s| {
        let (first, span) = s.row_span_mut(0, 0, 8).expect("row admitted");
        assert_eq!(first, 3);
        assert_eq!(span.len(), 2);
        assert!(s.row_span_mut(1, 6, 2).is_none(), "columns past the window");
    });
    assert!(
        s.row_span_mut(1, 6, 2).is_some(),
        "the window is restored on return"
    );
}

/// A nested window can only narrow: a control handed a clipped surface must
/// not be able to paint its way back out to what its host withheld.
#[test]
fn a_nested_clip_can_only_narrow() {
    let mut s = Surface::new(4, 4).expect("allocates");
    s.with_clip(1, 1, 2, 2, |s| {
        s.with_clip(0, 0, 4, 4, |s| s.fill_rect(0, 0, 4, 4, RED));
    });
    assert_eq!(s.get(1, 1), Some(RED.premultiply()));
    assert_eq!(s.get(0, 0), Some(Pixel::TRANSPARENT));
    assert_eq!(s.get(3, 3), Some(Pixel::TRANSPARENT));
}

#[test]
fn an_empty_clip_admits_nothing_and_restores() {
    let mut s = Surface::new(2, 2).expect("allocates");
    s.with_clip(0, 0, 0, 0, |s| s.fill(RED));
    assert!(s.pixels().iter().all(|p| *p == Pixel::TRANSPARENT));
    // A window entirely off-surface is equally empty, not wrapped.
    s.with_clip(9, 9, 4, 4, |s| s.fill(RED));
    assert!(s.pixels().iter().all(|p| *p == Pixel::TRANSPARENT));
    s.fill(BLUE);
    assert!(s.pixels().iter().all(|p| *p == BLUE.premultiply()));
}

// ---- CachedBytes: Surface plugs into the shared reclaim cache --------

#[test]
fn payload_bytes_matches_the_pixel_buffer_size() {
    let surface = Surface::new(4, 5).expect("allocates");
    assert_eq!(
        surface.payload_bytes(),
        (4 * 5) * core::mem::size_of::<Pixel>()
    );
}

#[test]
fn wipe_clears_every_pixel() {
    let mut surface = Surface::new(2, 2).expect("allocates");
    surface.fill(RED);
    assert!(surface.pixels().iter().all(|p| *p == RED.premultiply()));
    surface.wipe();
    assert!(surface.pixels().iter().all(|p| *p == Pixel::TRANSPARENT));
}

// ---- vertical gradient ----------------------------------------------

#[test]
fn a_vertical_gradient_ramps_from_the_top_colour_to_the_bottom_one() {
    let mut s = Surface::new(1, 5).expect("allocates");
    s.fill_vertical_gradient(0, 0, 1, 5, RED, BLUE);

    // The ends are exactly the authored colours, so a caller's chosen stops
    // survive the ramp rather than being approached.
    assert_eq!(s.get(0, 0), Some(RED.premultiply()));
    assert_eq!(s.get(0, 4), Some(BLUE.premultiply()));
    // In between the ramp is monotone in both channels.
    for y in 1..5 {
        let above = s.get(0, y - 1).expect("in bounds");
        let here = s.get(0, y).expect("in bounds");
        assert!(here.r < above.r, "red rose at row {y}");
        assert!(here.b > above.b, "blue fell at row {y}");
    }
}

#[test]
fn a_gradient_that_fades_out_keeps_its_hue_all_the_way_down() {
    // Interpolating premultiplied instead of straight alpha would drag the
    // colour toward black as the alpha fell; the hue must stay put.
    let white = Color::rgb(255, 255, 255);
    let mut s = Surface::new(1, 5).expect("allocates");
    s.fill_vertical_gradient(0, 0, 1, 5, white, Color::rgba(255, 255, 255, 0));

    let mut previous = 255;
    for y in 0..4 {
        let pixel = s.get(0, y).expect("in bounds");
        assert!(pixel.a < previous || y == 0, "alpha rose at row {y}");
        previous = pixel.a;
        assert_eq!(pixel.unpremultiply(), Color::rgba(255, 255, 255, pixel.a));
    }
    assert_eq!(s.get(0, 4), Some(Pixel::TRANSPARENT));
}

#[test]
fn a_one_row_gradient_is_the_top_colour_and_an_empty_one_draws_nothing() {
    let mut s = Surface::new(1, 1).expect("allocates");
    s.fill_vertical_gradient(0, 0, 1, 1, RED, BLUE);
    assert_eq!(s.get(0, 0), Some(RED.premultiply()));

    let mut empty = Surface::new(2, 2).expect("allocates");
    empty.fill_vertical_gradient(0, 0, 0, 2, RED, BLUE);
    empty.fill_vertical_gradient(0, 0, 2, 0, RED, BLUE);
    assert!(empty.pixels().iter().all(|p| *p == Pixel::TRANSPARENT));
}

#[test]
fn a_clipped_gradient_keeps_the_ramp_the_whole_rectangle_would_have_had() {
    let mut whole = Surface::new(1, 6).expect("allocates");
    whole.fill_vertical_gradient(0, 0, 1, 6, RED, BLUE);

    let mut clipped = Surface::new(1, 6).expect("allocates");
    clipped.with_clip(0, 2, 1, 2, |s| {
        s.fill_vertical_gradient(0, 0, 1, 6, RED, BLUE);
    });
    for y in 2..4 {
        assert_eq!(clipped.get(0, y), whole.get(0, y), "row {y} re-scaled");
    }
    assert_eq!(clipped.get(0, 1), Some(Pixel::TRANSPARENT));
    assert_eq!(clipped.get(0, 4), Some(Pixel::TRANSPARENT));
}

// ---- rounded-rect mask ----------------------------------------------

#[test]
fn masking_to_a_rectangle_clears_everything_outside_it() {
    let mut s = Surface::new(8, 8).expect("allocates");
    s.fill(RED);
    s.mask_to_round_rect(2, 2, 4, 4, 0);

    for y in 0..8 {
        for x in 0..8 {
            let inside = (2..6).contains(&x) && (2..6).contains(&y);
            let expected = if inside {
                RED.premultiply()
            } else {
                Pixel::TRANSPARENT
            };
            assert_eq!(s.get(x, y), Some(expected), "at ({x}, {y})");
        }
    }
}

#[test]
fn a_mask_rounds_through_the_same_coverage_a_fill_does() {
    // A masked shape and a filled one must have identical edges, or content
    // rounded after the fact would not sit inside the shape drawn for it.
    let mut s = Surface::new(9, 7).expect("allocates");
    s.fill(RED);
    s.mask_to_round_rect(0, 0, 9, 7, 3);

    for y in 0..7 {
        for x in 0..9 {
            let coverage = round_rect_coverage(x, y, 9, 7, 3);
            assert_eq!(s.get(x, y).expect("in bounds").a, coverage, "at ({x}, {y})");
        }
    }
}

#[test]
fn masking_to_half_the_height_yields_a_stadium() {
    let mut s = Surface::new(12, 6).expect("allocates");
    s.fill(RED);
    s.mask_to_round_rect(0, 0, 12, 6, 3);

    // The waist reaches both ends — the arc is tangent there, so that pixel
    // is nearly, not exactly, whole — while the corners are cut away and the
    // middle is untouched.
    assert!(s.get(0, 3).expect("in bounds").a > 200);
    assert!(s.get(11, 3).expect("in bounds").a > 200);
    assert_eq!(s.get(6, 3), Some(RED.premultiply()));
    assert_eq!(s.get(0, 0), Some(Pixel::TRANSPARENT));
    assert_eq!(s.get(11, 5), Some(Pixel::TRANSPARENT));
}

#[test]
fn masking_to_nothing_clears_the_surface() {
    let mut s = Surface::new(4, 4).expect("allocates");
    s.fill(RED);
    s.mask_to_round_rect(1, 1, 0, 0, 0);
    assert!(s.pixels().iter().all(|p| *p == Pixel::TRANSPARENT));
}

// ---- layered composition --------------------------------------------

/// Two rectangles that abut exactly at x = 4.5 pixels, painted one over the
/// other as two layers of one stack.
fn abutting_halves(width: u32, height: u32) -> Option<Surface> {
    let left = alloc::vec![alloc::vec![(0, 0), (9, 0), (9, 20), (0, 20)]];
    let right = alloc::vec![alloc::vec![(9, 0), (20, 0), (20, 20), (9, 20)]];
    Surface::layered(width, height, 2, |surface| {
        surface.fill_contours(&left, 20, FillRule::NonZero, &Paint::Solid(RED));
        surface.fill_contours(&right, 20, FillRule::NonZero, &Paint::Solid(RED));
    })
}

#[test]
fn abutting_layers_leave_no_pale_seam() {
    // The two halves together cover the shared column completely, but each
    // covers only part of it. Composited at the result's own resolution their
    // partial alphas blend as if they overlapped and the seam comes out short
    // of opaque — the washed-out look. Painting the stack larger and averaging
    // it down is what recovers the full coverage.
    let s = abutting_halves(10, 4).expect("allocates");
    for y in 0..4 {
        for x in 0..10 {
            assert_eq!(s.get(x, y), Some(RED.premultiply()), "at ({x}, {y})");
        }
    }
}

#[test]
fn a_layered_composite_is_exactly_the_size_asked_for() {
    let s = abutting_halves(13, 7).expect("allocates");
    assert_eq!((s.width(), s.height()), (13, 7));
}

#[test]
fn a_lone_layer_is_painted_at_its_own_size() {
    // One layer meets nothing, so there is no seam to resolve and no reason to
    // pay for an enlargement: the result must be what painting it directly
    // gives, pixel for pixel.
    let triangle = alloc::vec![alloc::vec![(1, 1), (19, 5), (7, 18)]];
    let paint = |surface: &mut Surface| {
        surface.fill_contours(&triangle, 20, FillRule::NonZero, &Paint::Solid(BLUE));
    };
    let layered = Surface::layered(12, 12, 1, paint).expect("allocates");
    let mut direct = Surface::new(12, 12).expect("allocates");
    paint(&mut direct);
    assert_eq!(layered, direct);
}

#[test]
fn a_layered_composite_of_no_size_is_empty_rather_than_enlarged() {
    // A degenerate extent is a legal, empty surface here, exactly as it is for
    // `Surface::new`: enlarging and averaging must not invent a size for it or
    // trip over the division back down.
    let bar = alloc::vec![alloc::vec![(0, 0), (20, 0), (20, 20), (0, 20)]];
    for (width, height) in [(0, 4), (4, 0), (0, 0)] {
        let s = Surface::layered(width, height, 2, |surface| {
            surface.fill_contours(&bar, 20, FillRule::NonZero, &Paint::Solid(RED));
        })
        .expect("an empty surface still allocates");
        assert_eq!((s.width(), s.height()), (width, height));
        assert!(s.pixels().is_empty());
    }
}

#[test]
fn a_large_layered_composite_is_painted_without_enlargement() {
    // Enlarging tapers off as the result gets finer: a seam a large surface
    // leaves is already a sliver of one pixel, so the cost stops being worth
    // paying and the stack is painted straight.
    let bar = alloc::vec![alloc::vec![(0, 0), (20, 0), (20, 9), (0, 9)]];
    let paint = |surface: &mut Surface| {
        surface.fill_contours(&bar, 20, FillRule::NonZero, &Paint::Solid(RED));
    };
    let layered = Surface::layered(300, 300, 2, paint).expect("allocates");
    let mut direct = Surface::new(300, 300).expect("allocates");
    paint(&mut direct);
    assert_eq!(layered, direct);
}
