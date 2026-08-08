//! Unit tests for the shared box blur.
//!
//! They pin down the identities every frosted surface relies on: a uniform
//! field survives exactly, an impulse spreads symmetrically and conserves
//! its energy, alpha blurs with the colour channels and keeps the
//! premultiplied invariant, and every degenerate shape (one pixel, one row,
//! one column, a radius wider than the region) is answered rather than
//! refused. The rest are the fail-closed refusals, and the equivalence of
//! the allocating [`Surface::blur`] with the scratch-owning [`box_blur`].

use alloc::vec;
use alloc::vec::Vec;

use super::box_blur;
use crate::color::Pixel;
use crate::surface::Surface;

/// An opaque grey.
fn grey(v: u8) -> Pixel {
    Pixel {
        r: v,
        g: v,
        b: v,
        a: 255,
    }
}

/// Blur `region` (given as `width`×`height`) and hand back the result.
fn blurred(region: &[Pixel], width: usize, height: usize, radius: usize) -> Vec<Pixel> {
    let mut pixels = region.to_vec();
    let mut aux = vec![Pixel::TRANSPARENT; width * height];
    box_blur(&mut pixels, width, height, radius, &mut aux);
    pixels
}

/// A `width`×`height` surface whose every pixel differs, so a blur that
/// reads or writes the wrong one cannot hide behind a flat field.
fn patterned(width: u32, height: u32) -> Surface {
    let mut surface = Surface::new(width, height).expect("allocates");
    for y in 0..height {
        for x in 0..width {
            let channel = |factor: u32| u8::try_from((x * factor + y * 13) % 256).unwrap_or(0);
            surface.set(
                x,
                y,
                Pixel {
                    r: channel(7).min(channel(53)),
                    g: channel(11).min(channel(53)),
                    b: channel(29).min(channel(53)),
                    a: channel(53),
                },
            );
        }
    }
    surface
}

#[test]
fn uniform_field_is_unchanged() {
    let field = vec![grey(40); 7 * 5];
    assert_eq!(blurred(&field, 7, 5, 2), field);
}

#[test]
fn radius_zero_is_identity() {
    let mut field = vec![grey(10); 9];
    field[4] = grey(200);
    assert_eq!(blurred(&field, 3, 3, 0), field);
}

#[test]
fn impulse_spreads_symmetrically() {
    let mut field = vec![grey(0); 5 * 5];
    field[2 * 5 + 2] = grey(255);
    let out = blurred(&field, 5, 5, 1);

    let centre = out[2 * 5 + 2];
    assert!(centre.r > 0 && centre.r < 255, "the impulse spread out");
    for (x, y) in [(1, 2), (3, 2), (2, 1), (2, 3)] {
        assert_eq!(
            out[y * 5 + x],
            centre,
            "a 3x3 box spreads an impulse equally over its whole window"
        );
    }
    assert_eq!(out[0], grey(0), "a pixel two columns away is untouched");
}

#[test]
fn impulse_conserves_total_energy() {
    let mut field = vec![Pixel::TRANSPARENT; 9 * 9];
    field[4 * 9 + 4] = grey(90);
    let out = blurred(&field, 9, 9, 1);
    let total: u32 = out.iter().map(|p| u32::from(p.r)).sum();
    assert_eq!(total, 90, "a box blur redistributes, it does not amplify");
}

#[test]
fn alpha_is_averaged_with_the_colour_channels() {
    let opaque = grey(200);
    let field = vec![
        opaque,
        opaque,
        opaque,
        Pixel::TRANSPARENT,
        Pixel::TRANSPARENT,
        Pixel::TRANSPARENT,
    ];
    let out = blurred(&field, 6, 1, 1);
    let edge = out[3];
    assert!(
        edge.a > 0 && edge.a < 255,
        "alpha blurs across the edge like every other channel"
    );
    assert!(
        edge.r <= edge.a,
        "the premultiplied invariant survives the blur"
    );
}

#[test]
fn single_pixel_region_is_returned_unchanged() {
    let field = vec![grey(77)];
    assert_eq!(blurred(&field, 1, 1, 4), field);
}

#[test]
fn single_column_and_single_row_regions_are_handled() {
    let column = vec![grey(0), grey(255), grey(0)];
    let out = blurred(&column, 1, 3, 3);
    assert_eq!(out[0], out[2], "a clamped column blurs symmetrically");
    assert!(out[1].r > 0 && out[1].r < 255);

    let row = vec![grey(0), grey(255), grey(0)];
    assert_eq!(blurred(&row, 3, 1, 3), out, "a row blurs like a column");
}

#[test]
fn radius_wider_than_the_region_still_averages() {
    let field = vec![grey(0), grey(120), grey(240), grey(0)];
    let out = blurred(&field, 4, 1, 64);
    assert!(
        out.windows(2).all(|pair| pair[0] == pair[1]),
        "a radius that swallows the region flattens it"
    );
}

#[test]
fn short_buffers_leave_the_region_untouched() {
    let field = vec![grey(10), grey(250), grey(10), grey(250)];
    let mut pixels = field.clone();
    let mut aux = vec![Pixel::TRANSPARENT; 2];
    box_blur(&mut pixels, 2, 2, 1, &mut aux[..1]);
    assert_eq!(pixels, field, "a short scratch buffer blurs nothing");

    let mut short = field[..3].to_vec();
    box_blur(&mut short, 2, 2, 1, &mut aux);
    assert_eq!(short, field[..3], "a short region blurs nothing");
}

#[test]
fn surface_blur_matches_the_scratch_owning_call() {
    for radius in [1u32, 2, 5] {
        let mut surface = patterned(9, 7);
        let expected = blurred(surface.pixels(), 9, 7, radius as usize);
        surface.blur(radius);
        assert_eq!(
            surface.pixels(),
            expected,
            "the allocating form is the same blur at radius {radius}"
        );
    }
}

#[test]
fn surface_blur_of_radius_zero_is_identity() {
    let untouched = patterned(6, 4);
    let mut surface = untouched.clone();
    surface.blur(0);
    assert_eq!(surface, untouched, "a disabled blur changes nothing");
}

#[test]
fn surface_blur_of_an_empty_surface_is_a_no_op() {
    for (w, h) in [(0, 0), (0, 5), (5, 0)] {
        let mut surface = Surface::new(w, h).expect("allocates");
        surface.blur(3);
        assert!(surface.pixels().is_empty(), "{w}x{h} has no pixels to blur");
    }
}

#[test]
fn an_absurd_radius_saturates_rather_than_panicking() {
    // Radii past the point where a channel times the window's sample count
    // leaves `u32`: the arithmetic saturates instead of overflowing.
    for radius in [u32::MAX / 255, u32::MAX / 2, u32::MAX] {
        let mut surface = patterned(5, 5);
        surface.blur(radius);
        assert!(
            surface
                .pixels()
                .iter()
                .all(|p| p.r <= p.a && p.g <= p.a && p.b <= p.a),
            "radius {radius} keeps the premultiplied invariant"
        );
    }
}
