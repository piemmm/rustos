//! Unit tests for the shared rasterisation primitives.

use crate::cache::RasterCache;
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

// ---- rasterisation cache --------------------------------------------

#[test]
fn cache_starts_empty_with_no_epoch() {
    let cache: RasterCache<u8, u32, u32> = RasterCache::new();
    assert!(cache.is_empty());
    assert_eq!(cache.len(), 0);
    assert_eq!(cache.epoch(), None);
}

#[test]
fn cache_renders_once_then_reuses_within_epoch() {
    let mut cache: RasterCache<u8, u32, u32> = RasterCache::new();
    let mut renders = 0;
    let first = *cache
        .get_or_render(&1, 7, || {
            renders += 1;
            Some(70)
        })
        .expect("rendered");
    assert_eq!(first, 70);
    // A second lookup of the same key at the same epoch does not re-render.
    let second = *cache
        .get_or_render(&1, 7, || {
            renders += 1;
            Some(999)
        })
        .expect("cached");
    assert_eq!(second, 70);
    assert_eq!(renders, 1);
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.epoch(), Some(&1));
}

#[test]
fn cache_keeps_distinct_keys_in_one_epoch() {
    let mut cache: RasterCache<u8, u32, u32> = RasterCache::new();
    assert_eq!(*cache.get_or_render(&1, 1, || Some(11)).expect("a"), 11);
    assert_eq!(*cache.get_or_render(&1, 2, || Some(22)).expect("b"), 22);
    assert_eq!(cache.len(), 2);
    // Both remain reachable without re-rendering.
    assert_eq!(*cache.get_or_render(&1, 1, || Some(0)).expect("a"), 11);
    assert_eq!(*cache.get_or_render(&1, 2, || Some(0)).expect("b"), 22);
}

#[test]
fn cache_invalidates_when_epoch_changes() {
    let mut cache: RasterCache<u8, u32, u32> = RasterCache::new();
    assert_eq!(*cache.get_or_render(&1, 7, || Some(70)).expect("a"), 70);
    // A new epoch (a scale or theme change) discards the old entries and
    // re-renders.
    assert_eq!(*cache.get_or_render(&2, 7, || Some(71)).expect("b"), 71);
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.epoch(), Some(&2));
}

#[test]
fn cache_does_not_remember_a_failed_render() {
    let mut cache: RasterCache<u8, u32, u32> = RasterCache::new();
    assert_eq!(cache.get_or_render(&1, 7, || None), None);
    assert!(cache.is_empty());
    // The next attempt is retried rather than the failure being cached.
    let mut renders = 0;
    assert_eq!(
        *cache
            .get_or_render(&1, 7, || {
                renders += 1;
                Some(70)
            })
            .expect("retried"),
        70
    );
    assert_eq!(renders, 1);
}

#[test]
fn cache_clear_drops_entries_and_epoch() {
    let mut cache: RasterCache<u8, u32, u32> = RasterCache::new();
    let _ = cache.get_or_render(&1, 7, || Some(70));
    cache.clear();
    assert!(cache.is_empty());
    assert_eq!(cache.epoch(), None);
}
