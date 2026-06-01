//! Unit tests for the shared rasterisation primitives.

use crate::color::{Color, Pixel};
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
fn theme_rgba_converts_to_color_by_field_move() {
    let rgba = rustos_theme::Rgba::new(10, 20, 30, 40);
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
