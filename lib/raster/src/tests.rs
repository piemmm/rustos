//! Unit tests for the shared rasterisation primitives.

use tairix_reclaim::CachedBytes;

use crate::color::{Color, Pixel};
use crate::round::round_rect_coverage;
use crate::surface::Surface;

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
