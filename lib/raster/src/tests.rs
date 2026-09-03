//! Unit tests for the shared rasterisation primitives.

use tairix_reclaim::CachedBytes;

use crate::color::{Color, Pixel};
use crate::dither::DitherRow;
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

/// A window's surface is close to a megabyte, so a machine short of memory
/// is what refuses one. Userland's heap answers exhaustion with a null
/// pointer, which an infallible growth turns into a process abort — the
/// desktop session's death under the 32-window pressure soak — so both
/// constructors reserve the pixels and hand the refusal back instead. The
/// extent here makes the reservation impossible arithmetically, so the
/// refusal is the allocator's answer rather than a property of the host's
/// free memory.
#[test]
fn a_surface_the_allocator_refuses_is_none_not_a_panic() {
    assert_eq!(Surface::new(u32::MAX, u32::MAX), None);
    assert_eq!(Surface::filled(u32::MAX, u32::MAX, RED.premultiply()), None);
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
            // A translucent composite rounds at the pixel's own share of the
            // surface's ordered dither, arc and interior alike.
            let bias = DitherRow::at(row).bias(col);
            surface.set(
                col,
                row,
                source
                    .scale_alpha_biased(coverage, bias)
                    .over_biased(dst, bias),
            );
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
fn overwrite_replaces_what_it_lands_on_where_a_blit_would_composite() {
    // A snapshot is a copy, not a composite: the compositor retains the
    // backdrop beneath a translucent or blurred window with this, and a
    // transparent source pixel must arrive as transparent rather than leave
    // the old contents showing.
    let mut copied = Surface::new(4, 4).expect("allocates");
    copied.fill(BLUE);
    let mut composited = Surface::new(4, 4).expect("allocates");
    composited.fill(BLUE);
    let mut src = Surface::new(2, 2).expect("allocates");
    src.set(0, 0, RED.premultiply());
    src.set(1, 0, Color::rgba(0, 200, 40, 128).premultiply());

    copied.overwrite(1, 1, &src);
    composited.blit(1, 1, &src);
    assert_eq!(copied.get(1, 1), Some(RED.premultiply()));
    assert_eq!(
        copied.get(2, 2),
        Some(Pixel::TRANSPARENT),
        "a transparent source pixel replaces, it does not skip"
    );
    assert_ne!(
        copied.pixels(),
        composited.pixels(),
        "the two differ exactly where the source was not opaque"
    );
    // Outside the footprint neither touched anything.
    assert_eq!(copied.get(0, 0), Some(BLUE.premultiply()));
}

#[test]
fn overwriting_a_whole_surface_reproduces_it_exactly() {
    // The property the retained backdrop rests on: what comes back is what
    // was there, byte for byte, translucent pixels included.
    let mut source = Surface::new(5, 3).expect("allocates");
    source.fill(BLUE);
    source.set(2, 1, Color::rgba(10, 20, 30, 90).premultiply());
    source.set(4, 2, Pixel::TRANSPARENT);
    let mut taken = Surface::new(5, 3).expect("allocates");
    taken.fill(RED);
    taken.overwrite(0, 0, &source);
    assert_eq!(taken.pixels(), source.pixels());
}

#[test]
fn an_overwrite_clips_exactly_as_a_plain_blit_does() {
    // The two share one geometry walk, so a source hanging off an edge must
    // land on precisely the same pixels either way.
    let mut src = Surface::new(3, 3).expect("allocates");
    src.fill(RED);
    for at in [(-2i32, -1i32), (3, 2), (-4, -4), (2, 0)] {
        let mut copied = Surface::new(4, 4).expect("allocates");
        copied.fill(BLUE);
        let mut blitted = Surface::new(4, 4).expect("allocates");
        blitted.fill(BLUE);
        copied.overwrite(at.0, at.1, &src);
        blitted.blit(at.0, at.1, &src);
        assert_eq!(
            copied.pixels(),
            blitted.pixels(),
            "an opaque source at {at:?} copies and composites alike"
        );
    }
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
fn a_faded_blit_mixes_the_destination_toward_an_opaque_source() {
    // What a crossfade rests on: half strength must land halfway between the
    // two pictures, not somewhere darker or lighter than either.
    let mut src = Surface::new(2, 2).expect("allocates");
    src.fill(RED);
    let mut dst = Surface::new(2, 2).expect("allocates");
    dst.fill(BLUE);
    dst.blit_faded(0, 0, &src, 128);
    let landed = dst.get(1, 1).expect("in bounds");
    let (red, blue) = (RED.premultiply(), BLUE.premultiply());
    for (got, want) in [
        (landed.r, red.r / 2),
        (landed.b, blue.b / 2),
        (landed.a, 255),
    ] {
        assert!(
            got.abs_diff(want) <= 1,
            "{got} is not within a level of {want}"
        );
    }
    // The picture itself is untouched, so neither end of a fade is copied.
    assert_eq!(src.get(0, 0), Some(red));
}

#[test]
fn a_full_strength_fade_is_the_plain_blit_and_a_zero_one_draws_nothing() {
    let mut src = Surface::new(3, 3).expect("allocates");
    src.fill(RED);
    src.set(1, 1, Color::rgba(0, 200, 40, 128).premultiply());
    let mut plain = Surface::new(3, 3).expect("allocates");
    plain.fill(BLUE);
    let mut faded = Surface::new(3, 3).expect("allocates");
    faded.fill(BLUE);
    plain.blit(0, 0, &src);
    faded.blit_faded(0, 0, &src, 255);
    assert_eq!(plain.pixels(), faded.pixels());

    let mut untouched = Surface::new(3, 3).expect("allocates");
    untouched.fill(BLUE);
    let before = untouched.pixels().to_vec();
    untouched.blit_faded(0, 0, &src, 0);
    assert_eq!(untouched.pixels(), before);
}

#[test]
fn a_faded_blit_clips_exactly_as_a_plain_one_does() {
    let mut src = Surface::new(4, 4).expect("allocates");
    src.fill(RED);
    for at in [(-1i32, -1i32), (1, 2), (-5, 0)] {
        let mut faded = Surface::new(2, 2).expect("allocates");
        faded.fill(BLUE);
        let mut plain = Surface::new(2, 2).expect("allocates");
        plain.fill(BLUE);
        faded.blit_faded(at.0, at.1, &src, 255);
        plain.blit(at.0, at.1, &src);
        assert_eq!(faded.pixels(), plain.pixels(), "a source at {at:?}");
    }
}

#[test]
fn wash_region_scales_the_source_by_the_mask_and_spares_bare_pixels() {
    let mut surface = Surface::new(8, 4).expect("allocates");
    surface.fill(BLUE);
    let before = surface.pixels().to_vec();
    let wash = Color::rgba(255, 0, 0, 255);

    // Column 0 uncovered, column 1 half covered, column 2 fully covered.
    surface.wash_region(0, 0, 3, 4, wash, |lx, _| match lx {
        0 => 0,
        1 => 128,
        _ => 255,
    });

    assert_eq!(
        surface.get(0, 0),
        Some(before[0]),
        "an uncovered pixel is left bit-identical, not blended with nothing"
    );
    assert_eq!(
        surface.get(2, 0),
        Some(wash.premultiply()),
        "full coverage composites the source outright"
    );
    let half = surface.get(1, 0).expect("in bounds");
    assert!(
        half.r > 0 && half.b > 0,
        "half coverage leaves both the wash and the ground: {half:?}"
    );
    assert_eq!(surface.get(7, 0), Some(before[7]), "and nothing outside it");
}

#[test]
fn wash_region_takes_the_mask_at_the_rectangles_own_coordinates() {
    // The mask is asked about the pixel's place in the *rectangle*, not in the
    // surface, so a caller composing a ramp with an arc writes one expression
    // whatever the rectangle's origin is.
    let mut offset = Surface::new(8, 8).expect("allocates");
    offset.fill(BLUE);
    offset.wash_region(3, 2, 4, 4, RED, |lx, ly| {
        u8::try_from((lx + ly) * 32).unwrap_or(255)
    });

    let mut origin = Surface::new(8, 8).expect("allocates");
    origin.fill(BLUE);
    origin.wash_region(0, 0, 4, 4, RED, |lx, ly| {
        u8::try_from((lx + ly) * 32).unwrap_or(255)
    });

    // Same mask answers, so the rectangle's own pixels match wherever it sits
    // (bar the dither, which tiles the surface and so differs by origin).
    for ly in 0..4 {
        for lx in 0..4 {
            let there = offset.get(3 + lx, 2 + ly).expect("in bounds");
            let here = origin.get(lx, ly).expect("in bounds");
            let close = |a: u8, b: u8| a.abs_diff(b) <= 1;
            assert!(
                close(there.r, here.r) && close(there.b, here.b),
                "({lx}, {ly}): {there:?} vs {here:?}"
            );
        }
    }
}

#[test]
fn wash_region_is_a_no_op_for_a_transparent_or_empty_wash() {
    let mut surface = Surface::new(4, 4).expect("allocates");
    surface.fill(BLUE);
    let before = surface.pixels().to_vec();
    surface.wash_region(0, 0, 4, 4, Color::rgba(255, 0, 0, 0), |_, _| 255);
    assert_eq!(surface.pixels(), &before[..], "a transparent wash");
    surface.wash_region(0, 0, 0, 4, RED, |_, _| 255);
    surface.wash_region(0, 0, 4, 0, RED, |_, _| 255);
    assert_eq!(surface.pixels(), &before[..], "an empty rectangle");
}

/// A `side`×`side` surface filled with `ground`, with a `patch`×`patch` block
/// of `accent` in its corner — a stand-in for an icon whose colour is a mark
/// on a neutral field.
fn icon(side: u32, ground: Color, patch: u32, accent: Color) -> Surface {
    let mut surface = Surface::new(side, side).expect("allocates");
    surface.fill(ground);
    surface.fill_rect(0, 0, patch, patch, accent);
    surface
}

#[test]
fn dominant_color_finds_the_accent_in_a_mostly_grey_icon() {
    // The colour a title bar takes its wash from is the icon's *hue*, not its
    // average: a grey glyph with a coloured mark reads as that colour.
    let accent = Color::rgb(0x0a, 0x93, 0xe6);
    let found = icon(16, Color::rgb(128, 128, 128), 8, accent)
        .dominant_color()
        .expect("the mark carries a hue");
    assert!(
        found.b > found.r && found.b > found.g,
        "the blue mark won, not the grey field: {found:?}"
    );
    assert_eq!(found.a, 255, "the hue is returned opaque");
}

#[test]
fn dominant_color_picks_a_hue_rather_than_averaging_two() {
    // Half red, half cyan averages to grey; the mode does not. Whichever side
    // carries more weight must come back as a real hue.
    let mut surface = Surface::new(16, 16).expect("allocates");
    surface.fill(Color::rgb(0, 255, 255));
    surface.fill_rect(0, 0, 16, 12, Color::rgb(255, 0, 0));
    let found = surface.dominant_color().expect("both halves have a hue");
    assert!(
        found.r > found.g && found.r > found.b,
        "the larger red field won instead of the two cancelling: {found:?}"
    );
}

#[test]
fn dominant_color_declines_when_there_is_no_hue_to_lend() {
    assert_eq!(
        icon(16, Color::rgb(60, 60, 60), 8, Color::rgb(200, 200, 200)).dominant_color(),
        None,
        "a greyscale icon lends no colour"
    );
    assert_eq!(
        Surface::new(16, 16).expect("allocates").dominant_color(),
        None,
        "and neither does a transparent one"
    );
    assert_eq!(
        icon(16, Color::rgb(128, 128, 128), 1, Color::rgb(255, 0, 0)).dominant_color(),
        None,
        "nor one whose only colour is a single stray pixel"
    );
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

// ---- a stated origin: one rectangle of a larger drawing --------------
//
// A surface may stand in for one rectangle of a drawing larger than itself,
// which is how a strip of window furniture is rendered without a
// window-sized buffer. Coordinates are then the drawing's; the primitives
// defined in terms of "the surface" act on the rectangle it holds.

/// The bands of `DRAWING_W`×`DRAWING_H` a window's furniture is cut to: the
/// full-width top and bottom, and the two side borders between them.
const BANDS: [(u32, u32, u32, u32); 4] =
    [(0, 0, 24, 4), (0, 16, 24, 4), (0, 4, 3, 12), (21, 4, 3, 12)];

const DRAWING_W: u32 = 24;
const DRAWING_H: u32 = 20;

/// A drawing exercising every position-dependent primitive at once — a
/// rounded fill's corner arcs, a gradient's ramp, a two-dimensional wash's
/// ordered dither, a placed sprite, device-space geometry, and a shape mask —
/// so a strip that differed anywhere in any of them shows it.
fn furniture_like_drawing(surface: &mut Surface, sprite: &Surface) {
    let step = SUBPIXEL;
    surface.fill_round_rect(0, 0, DRAWING_W, DRAWING_H, 6, Color::rgba(30, 60, 90, 255));
    surface.fill_vertical_gradient(
        2,
        1,
        DRAWING_W - 4,
        DRAWING_H - 2,
        Color::rgba(255, 0, 0, 90),
        Color::rgba(0, 0, 255, 210),
    );
    surface.wash_region(
        0,
        0,
        DRAWING_W,
        DRAWING_H,
        Color::rgba(0, 255, 0, 130),
        |x, y| u8::try_from((x * 11 + y * 5) % 251).unwrap_or(0),
    );
    surface.blit(19, 15, sprite);
    surface.fill_polygon_subpixel(
        &[
            (6 * step, 2 * step),
            (20 * step, 9 * step),
            (3 * step, 17 * step),
        ],
        RED,
    );
    surface.stroke_polyline(
        &[(0, 0), (24 * step, 20 * step)],
        step,
        Color::rgba(255, 255, 255, 160),
    );
    surface.mask_to_round_rect(1, 1, DRAWING_W - 2, DRAWING_H - 2, 4);
}

/// A small sprite with a distinct pixel per position, so a blit landing one
/// column or row out is visible.
fn sprite() -> Surface {
    let mut sprite = Surface::new(3, 3).expect("allocates");
    for y in 0..3 {
        for x in 0..3 {
            let level = 40 + 20 * u8::try_from(y * 3 + x).expect("small");
            sprite.set(x, y, Color::rgba(255, 255, 0, level).premultiply());
        }
    }
    sprite
}

/// The property every strip render rests on: a buffer standing in for one
/// rectangle of a drawing carries exactly the pixels that rectangle of the
/// whole drawing carries.
#[test]
fn a_strip_of_a_drawing_matches_that_rectangle_of_the_whole() {
    let sprite = sprite();
    let mut whole = Surface::new(DRAWING_W, DRAWING_H).expect("allocates");
    furniture_like_drawing(&mut whole, &sprite);

    for (x, y, w, h) in BANDS {
        let mut strip = Surface::new(w, h).expect("allocates");
        strip.with_origin(x, y, |s| furniture_like_drawing(s, &sprite));
        for row in 0..h {
            for column in 0..w {
                assert_eq!(
                    strip.get(column, row),
                    whole.get(x + column, y + row),
                    "band ({x}, {y}) at ({column}, {row})"
                );
            }
        }
        assert!(
            strip.pixels().iter().any(|p| *p != Pixel::TRANSPARENT),
            "band ({x}, {y}) is painted, so the comparison is not vacuous"
        );
    }
}

#[test]
fn a_stated_origin_maps_the_drawings_pixel_to_the_buffers_first() {
    let mut s = Surface::new(2, 2).expect("allocates");
    s.with_origin(10, 20, |s| s.set(10, 20, RED.premultiply()));
    assert_eq!(s.get(0, 0), Some(RED.premultiply()));
    assert_eq!(s.get(1, 1), Some(Pixel::TRANSPARENT));
}

#[test]
fn paint_outside_the_stated_rectangle_is_dropped() {
    let mut s = Surface::new(2, 2).expect("allocates");
    s.with_origin(10, 10, |s| {
        s.fill_rect(0, 0, 4, 4, RED);
        s.fill_rect(12, 12, 4, 4, RED);
    });
    assert!(s.pixels().iter().all(|p| *p == Pixel::TRANSPARENT));
}

/// A span reaching in from left of the buffer is cut, and reports the
/// *drawing* column it starts at — a caller pairing it with its own source
/// data advances that source by the difference from what it asked for.
#[test]
fn a_span_straddling_the_stated_origin_is_cut_and_reports_the_drawing_column() {
    let mut s = Surface::new(4, 1).expect("allocates");
    s.with_origin(6, 0, |s| {
        let (first, span) = s.row_span_mut(0, 4, 8).expect("row admitted");
        assert_eq!(first, 6, "the first drawing column the buffer holds");
        assert_eq!(span.len(), 4, "the columns left of the buffer are cut");
        assert!(s.row_span_mut(0, 0, 6).is_none(), "wholly left of it");
        assert!(s.row_span_mut(0, 10, 4).is_none(), "wholly right of it");
        assert!(s.row_span_mut(1, 6, 4).is_none(), "wholly below it");
    });
}

#[test]
fn the_stated_origin_is_restored_on_return() {
    let mut s = Surface::new(2, 2).expect("allocates");
    s.with_origin(5, 5, |s| s.set(5, 5, RED.premultiply()));
    s.set(1, 1, BLUE.premultiply());
    assert_eq!(s.get(0, 0), Some(RED.premultiply()));
    assert_eq!(s.get(1, 1), Some(BLUE.premultiply()));
}

#[test]
fn a_clip_inside_a_stated_origin_confines_in_the_drawings_coordinates() {
    let mut s = Surface::new(4, 4).expect("allocates");
    s.with_origin(10, 10, |s| {
        s.with_clip(12, 12, 2, 2, |s| s.fill_rect(10, 10, 4, 4, RED));
    });
    assert_eq!(s.get(2, 2), Some(RED.premultiply()));
    assert_eq!(s.get(1, 1), Some(Pixel::TRANSPARENT));
}

/// A restated origin relabels this buffer; it can never reach the pixels an
/// enclosing clip window withheld.
#[test]
fn a_restated_origin_reaches_no_further_than_the_buffer() {
    let mut s = Surface::new(2, 2).expect("allocates");
    s.with_clip(0, 0, 1, 1, |s| {
        s.with_origin(50, 50, |s| s.fill_rect(50, 50, 2, 2, RED));
    });
    assert_eq!(s.get(0, 0), Some(RED.premultiply()));
    assert_eq!(s.get(1, 1), Some(Pixel::TRANSPARENT));
}

/// "The surface" is the rectangle it holds, so a whole-surface fill still
/// covers every pixel of the buffer.
#[test]
fn a_whole_surface_fill_covers_the_buffer_under_a_stated_origin() {
    let mut s = Surface::new(3, 3).expect("allocates");
    s.with_origin(100, 100, |s| s.fill(RED));
    assert!(s.pixels().iter().all(|p| *p == RED.premultiply()));
}

/// A design-grid fill is likewise stretched over the surface's own rectangle
/// of the drawing, so it draws the same shape wherever that rectangle sits.
#[test]
fn a_design_grid_fill_stretches_across_the_surfaces_own_rectangle() {
    let square = [(0, 0), (4, 0), (4, 4), (0, 4)];
    let mut plain = Surface::new(4, 4).expect("allocates");
    plain.fill_polygon(&square, 4, RED);
    let mut placed = Surface::new(4, 4).expect("allocates");
    placed.with_origin(9, 5, |s| s.fill_polygon(&square, 4, RED));
    assert_eq!(placed, plain);
}

#[test]
fn a_blit_is_placed_in_the_drawings_coordinates() {
    let mut src = Surface::new(2, 1).expect("allocates");
    src.set(0, 0, RED.premultiply());
    src.set(1, 0, BLUE.premultiply());
    let mut s = Surface::new(2, 1).expect("allocates");
    // The sprite's first column is left of the buffer, so its second lands
    // in the buffer's first and the sprite is not smeared across.
    s.with_origin(7, 3, |s| s.blit(6, 3, &src));
    assert_eq!(s.get(0, 0), Some(BLUE.premultiply()));
    assert_eq!(s.get(1, 0), Some(Pixel::TRANSPARENT));
}

/// The alpha floor bounds the *buffer's* pixels, so a translated write has to
/// be recorded where those pixels are rather than where the drawing put them.
#[test]
fn row_bands_under_a_stated_origin_own_the_drawings_rows() {
    let mut s = Surface::new(2, 4).expect("allocates");
    s.with_origin(5, 7, |s| {
        let mut expected = 7;
        for mut band in s.row_bands_mut(7..11, 2) {
            let rows = band.rows();
            assert_eq!(rows, expected..expected + 2);
            assert!(
                band.row_span_mut(rows.end, 5, 2).is_none(),
                "a row the band does not own"
            );
            for row in rows {
                let (column, span) = band.row_span_mut(row, 5, 2).expect("row admitted");
                assert_eq!(column, 5);
                span.fill(RED.premultiply());
            }
            expected += 2;
        }
        assert_eq!(expected, 11, "four rows in bands of two");
    });
    assert!(s.pixels().iter().all(|p| *p == RED.premultiply()));
}

#[test]
fn admits_answers_for_the_stated_rectangle() {
    let mut s = Surface::new(4, 4).expect("allocates");
    assert!(s.admits(0, 0, 1, 1));
    assert!(!s.admits(4, 0, 1, 1));
    s.with_origin(10, 10, |s| {
        assert!(s.admits(13, 13, 1, 1));
        assert!(!s.admits(9, 9, 1, 1), "wholly above and left of it");
        assert!(!s.admits(14, 10, 4, 4), "wholly right of it");
        assert!(s.admits(8, 8, 4, 4), "a rectangle that reaches in");
    });
    s.with_clip(0, 0, 1, 1, |s| assert!(!s.admits(2, 2, 1, 1)));
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

/// A picture for a wash to be laid over: an opaque vertical ramp, one grey
/// level per row.
fn grey_ramp(width: u32, height: u32) -> Surface {
    let mut picture = Surface::new(width, height).expect("allocates");
    for y in 0..height {
        let level = u8::try_from(y).unwrap_or(u8::MAX);
        picture.fill_rect(0, y, width, 1, Color::rgb(level, level, level));
    }
    picture
}

/// One row's *area* tone: the sum of its green channels, which resolves the
/// row to a fraction of a level rather than to the one level a single pixel
/// can hold.
fn row_tone(surface: &Surface, y: u32) -> u32 {
    (0..surface.width())
        .filter_map(|x| surface.get(x, y))
        .map(|pixel| u32::from(pixel.g))
        .sum()
}

/// The longest run of consecutive rows of `surface` carrying the same area
/// tone — a flat plateau, which is what a band is — and how many distinct
/// tones the rows hold between them.
///
/// The scenes below ramp in one direction and a wash is monotone in what it
/// covers, so a tone that changes is a tone not seen before and counting the
/// changes counts the distinct tones.
fn tone_plateaus(surface: &Surface) -> (u32, u32) {
    let mut previous = row_tone(surface, 0);
    let (mut longest, mut run, mut tones) = (1, 1, 1);
    for y in 1..surface.height() {
        let tone = row_tone(surface, y);
        if tone == previous {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 1;
            tones += 1;
            previous = tone;
        }
    }
    (longest, tones)
}

#[test]
fn a_heavy_wash_over_a_ramp_keeps_the_ramp_distinguishable() {
    // A wash of alpha `a` has only `256 - a` levels to say what the picture
    // under it said in 256, so rounding every pixel the same way flattens a
    // slow ramp into wide plateaus with a step between them — the banding a
    // darkened wallpaper showed. Spreading that rounding across the area
    // keeps neighbouring rows apart: at alpha 224 the plateaus were nine rows
    // deep and only nine of 64 rows were distinguishable at all.
    for alpha in [176, 200, 224] {
        let mut washed = grey_ramp(64, 64);
        let wash = Color::rgba(11, 14, 16, alpha);
        washed.fill_vertical_gradient(0, 0, 64, 64, wash, wash);

        let (plateau, tones) = tone_plateaus(&washed);
        assert!(plateau <= 3, "alpha {alpha} flattened {plateau} rows");
        assert!(tones >= 24, "alpha {alpha} left only {tones} of 64 tones");
    }
}

#[test]
fn a_wash_spreads_its_rounding_error_over_the_area() {
    // A flat wash over a flat picture whose exact result falls between two
    // levels: one tile's pixels must average to that result rather than all
    // taking the nearer level, which is the tonal resolution the ramp above
    // survives on. Exactly `100 * 127 / 255 = 49.8` per pixel here, so the
    // tile of 64 owes 3187.45 and a fixed rounding pays 3200.
    let half_black = Color::rgba(0, 0, 0, 128);
    let mut washed = Surface::new(8, 8).expect("allocates");
    washed.fill(Color::rgb(100, 100, 100));
    washed.fill_vertical_gradient(0, 0, 8, 8, half_black, half_black);

    let tile: u32 = (0..8).map(|y| row_tone(&washed, y)).sum();
    let owed = 100 * 127 * 64 / 255;
    assert!(
        tile.abs_diff(owed) <= 1,
        "the tile paid {tile} against {owed} owed"
    );
}

#[test]
fn a_wash_of_the_colour_underneath_it_leaves_the_surface_exactly_as_it_was() {
    // The flat desktop backdrop behind the login column is washed in its own
    // colour, so the wash must be an identity there whatever its alpha:
    // dithering only the picture's share of the blend would sprinkle a
    // half-level of noise over a surface that has nothing to smooth.
    let desktop = Color::rgb(11, 14, 16);
    for alpha in [1, 64, 128, 140, 224, 254] {
        let mut surface = Surface::new(8, 8).expect("allocates");
        surface.fill(desktop);
        let wash = Color::rgba(desktop.r, desktop.g, desktop.b, alpha);
        surface.fill_vertical_gradient(0, 0, 8, 8, wash, wash);

        for y in 0..8 {
            for x in 0..8 {
                assert_eq!(
                    surface.get(x, y),
                    Some(desktop.premultiply()),
                    "alpha {alpha} at ({x}, {y})"
                );
            }
        }
    }
}

#[test]
fn a_wash_leaves_every_pixel_premultiplied() {
    // Every channel takes the same rounding bias, so a colour channel can
    // never round above the alpha it is premultiplied by — including over a
    // destination that is itself translucent.
    let mut surface = Surface::new(8, 8).expect("allocates");
    for y in 0..8 {
        let ground = Color::rgba(255, 200, 100, u8::try_from(y * 36).unwrap_or(u8::MAX));
        surface.fill_rect(0, y, 8, 1, ground);
    }
    let wash = Color::rgba(200, 150, 90, 96);
    surface.fill_vertical_gradient(0, 0, 8, 8, wash, Color::rgba(30, 40, 50, 200));

    for y in 0..8 {
        for x in 0..8 {
            let pixel = surface.get(x, y).expect("in bounds");
            assert!(
                pixel.r <= pixel.a && pixel.g <= pixel.a && pixel.b <= pixel.a,
                "({x}, {y}) came out unpremultiplied: {pixel:?}"
            );
        }
    }
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

// ---- row bands ------------------------------------------------------

/// Paint a surface a row at a time through `Surface::row_span_mut`, then paint
/// an identical one through bands, and require the two to be the same pixels.
/// This is the property every parallel pass rests on.
fn banded_and_whole_agree(width: u32, height: u32, rows_per_band: u32) {
    let paint = |y: u32, span: &mut [Pixel]| {
        for (x, pixel) in span.iter_mut().enumerate() {
            let x = u32::try_from(x).unwrap_or(0);
            *pixel = Color::rgb((x % 251) as u8, (y % 253) as u8, 42).premultiply();
        }
    };

    let mut whole = Surface::new(width, height).expect("allocates");
    for y in 0..height {
        if let Some((_, span)) = whole.row_span_mut(y, 0, width) {
            paint(y, span);
        }
    }

    let mut banded = Surface::new(width, height).expect("allocates");
    let mut covered = 0u32;
    for mut band in banded.row_bands_mut(0..height, rows_per_band) {
        let rows = band.rows();
        covered += rows.end - rows.start;
        for y in rows {
            if let Some((_, span)) = band.row_span_mut(y, 0, width) {
                paint(y, span);
            }
        }
    }
    assert_eq!(covered, height, "the bands must partition the rows");
    assert_eq!(banded, whole);
}

#[test]
fn bands_partition_the_rows_and_paint_what_the_whole_surface_would() {
    // Includes sizes that do not divide the height, a band per row, and the
    // whole range in one band (the serial case a single-CPU machine takes).
    for rows_per_band in [1, 2, 3, 4, 7, 16, 64] {
        banded_and_whole_agree(9, 7, rows_per_band);
    }
}

#[test]
fn a_band_refuses_a_row_it_does_not_own() {
    let mut surface = Surface::new(4, 6).expect("allocates");
    let mut bands = surface.row_bands_mut(0..6, 2);
    let mut first = bands.next().expect("two rows of six");
    assert_eq!(first.rows(), 0..2);
    assert!(first.row_span_mut(0, 0, 4).is_some());
    assert!(first.row_span_mut(1, 0, 4).is_some());
    assert!(
        first.row_span_mut(2, 0, 4).is_none(),
        "a band must not reach into the next band's rows"
    );
}

#[test]
fn a_band_of_a_row_subrange_starts_where_it_was_asked_to() {
    let mut surface = Surface::new(4, 8).expect("allocates");
    let spans: alloc::vec::Vec<_> = surface
        .row_bands_mut(3..7, 2)
        .map(|band| band.rows())
        .collect();
    assert_eq!(spans, alloc::vec![3..5, 5..7]);
}

#[test]
fn no_rows_no_bands() {
    let mut surface = Surface::new(4, 4).expect("allocates");
    assert_eq!(surface.row_bands_mut(2..2, 4).count(), 0);
    // A zero band size reads as one row per band rather than dividing by zero.
    assert_eq!(surface.row_bands_mut(0..4, 0).count(), 4);
    // Rows past the end are dropped rather than fabricated.
    assert_eq!(surface.row_bands_mut(9..12, 2).count(), 0);
    let mut empty = Surface::new(0, 0).expect("an empty surface still allocates");
    assert_eq!(empty.row_bands_mut(0..1, 2).count(), 0);
}

/// A band carries the surface's active clip window, so a clipped parallel pass
/// withholds exactly the pixels a clipped serial one does.
#[test]
fn a_band_honours_the_clip_window_the_surface_carries() {
    let mut surface = Surface::new(8, 8).expect("allocates");
    surface.with_clip(2, 1, 4, 5, |clipped| {
        let bands: alloc::vec::Vec<_> = clipped
            .row_bands_mut(0..8, 3)
            .map(|band| band.rows())
            .collect();
        assert_eq!(bands, alloc::vec![1..4, 4..6], "clipped rows only");
    });
    // Outside the clipped paint the window is the whole surface again, so a band
    // of it admits every column.
    let mut whole = surface.row_bands_mut(0..8, 8);
    let mut band = whole.next().expect("the whole surface");
    let (first, span) = band.row_span_mut(0, 0, 8).expect("row zero");
    assert_eq!((first, span.len()), (0, 8));
}

/// The read-only row span admits exactly what the writable one does, which is
/// what lets a blur read a neighbourhood while several of its pieces run.
#[test]
fn the_read_only_row_span_admits_what_the_writable_one_does() {
    let mut surface = Surface::new(6, 4).expect("allocates");
    surface.set(3, 2, Color::rgb(9, 9, 9).premultiply());
    for y in [0, 2, 3, 4, 9] {
        for (x, w) in [(0, 6), (1, 3), (5, 4), (6, 1), (0, 0)] {
            let readable = surface.row_span(y, x, w).map(|(at, span)| (at, span.len()));
            let writable = surface
                .row_span_mut(y, x, w)
                .map(|(at, span)| (at, span.len()));
            assert_eq!(readable, writable, "row {y}, columns {x}+{w}");
        }
    }
    let (_, span) = surface.row_span(2, 0, 6).expect("row two");
    assert_eq!(span[3], Color::rgb(9, 9, 9).premultiply());
}

/// A shape drawn as a tinted coverage mask is pixel-for-pixel what filling it
/// in that colour produces — the equivalence that lets a glyph be rasterised
/// once, untinted, and drawn in any theme colour.
#[test]
fn a_tinted_mask_blit_equals_filling_the_same_shape_in_that_colour() {
    // A triangle: axis-aligned edges give whole coverage, the diagonal gives
    // partial, so the comparison covers both.
    let triangle = alloc::vec![alloc::vec![(2, 2), (18, 2), (2, 18)]];
    let tint = Color::rgba(40, 160, 220, 255);

    let mut filled = Surface::new(20, 20).expect("surface");
    filled.fill_contours(&triangle, 20, FillRule::NonZero, &Paint::Solid(tint));

    // The mask is the same shape in opaque white, so its alpha *is* the
    // coverage; blitting it tinted must reproduce the fill.
    let mut mask = Surface::new(20, 20).expect("surface");
    mask.fill_contours(
        &triangle,
        20,
        FillRule::NonZero,
        &Paint::Solid(Color::rgba(255, 255, 255, 255)),
    );
    let mut blitted = Surface::new(20, 20).expect("surface");
    blitted.blit_tinted(0, 0, &mask, tint);

    for y in 0..20 {
        for x in 0..20 {
            assert_eq!(
                blitted.get(x, y),
                filled.get(x, y),
                "pixel ({x}, {y}) differs between a tinted mask and a direct fill"
            );
        }
    }
}

/// The equivalence holds over a non-empty destination too: the tint composites
/// *over* what is already there, and a pixel the mask does not cover is left
/// exactly as it was.
#[test]
fn a_tinted_mask_blit_composites_over_what_is_already_drawn() {
    let square = alloc::vec![alloc::vec![(4, 4), (12, 4), (12, 12), (4, 12)]];
    let tint = Color::rgba(200, 30, 30, 128);
    let ground = Pixel {
        r: 10,
        g: 20,
        b: 30,
        a: 255,
    };

    let mut filled = Surface::filled(16, 16, ground).expect("surface");
    filled.fill_contours(&square, 16, FillRule::NonZero, &Paint::Solid(tint));

    let mut mask = Surface::new(16, 16).expect("surface");
    mask.fill_contours(
        &square,
        16,
        FillRule::NonZero,
        &Paint::Solid(Color::rgba(255, 255, 255, 255)),
    );
    let mut blitted = Surface::filled(16, 16, ground).expect("surface");
    blitted.blit_tinted(0, 0, &mask, tint);

    for y in 0..16 {
        for x in 0..16 {
            assert_eq!(blitted.get(x, y), filled.get(x, y), "pixel ({x}, {y})");
        }
    }
    // A corner the square never reaches keeps the ground untouched.
    assert_eq!(blitted.get(0, 0), Some(ground));
}

/// A mask blit is placed and clipped exactly as any other blit: it is the same
/// walk, so an off-surface offset draws only the part that lands.
#[test]
fn a_tinted_mask_blit_clips_like_every_other_blit() {
    let mut mask = Surface::filled(
        4,
        4,
        Pixel {
            r: 255,
            g: 255,
            b: 255,
            a: 255,
        },
    )
    .expect("surface");
    mask.set(
        0,
        0,
        Pixel {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        },
    );
    let tint = Color::rgba(90, 90, 90, 255);
    let mut surface = Surface::new(6, 6).expect("surface");
    // Two columns and two rows hang off the top-left corner.
    surface.blit_tinted(-2, -2, &mask, tint);
    // The covered part landed...
    assert_eq!(surface.get(0, 0).map(|p| p.a), Some(255));
    assert_eq!(surface.get(1, 1).map(|p| p.a), Some(255));
    // ...and nothing was drawn where the mask does not reach.
    assert_eq!(surface.get(2, 2).map(|p| p.a), Some(0));
}
