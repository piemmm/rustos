//! Fur tests: the falloff table's shape, the splat's coverage and bounds, and
//! the ripple's determinism.

use super::{falloff_table, ripple, shadow, splat, Blob, FALLOFF_STEPS, RIPPLE_SECTORS};
use tairix_raster::{Color, Pixel, Surface};

const OPAQUE_RED: Color = Color::rgb(255, 0, 0);

fn blank(side: u32) -> Surface {
    Surface::filled(side, side, Pixel::TRANSPARENT).expect("a test surface allocates")
}

fn covered(surface: &Surface) -> usize {
    surface.pixels().iter().filter(|p| p.a > 0).count()
}

#[test]
fn the_falloff_is_solid_at_the_centre_and_empty_at_the_rim() {
    let table = falloff_table();
    assert_eq!(table[0], 255, "the middle of a blob is fully covered");
    assert_eq!(
        table[FALLOFF_STEPS - 1],
        0,
        "and the rim is not, or every blob would tint its bounding box"
    );
}

#[test]
fn the_falloff_never_rises_from_the_centre_outwards() {
    let table = falloff_table();
    for pair in table.windows(2) {
        assert!(
            pair[0] >= pair[1],
            "coverage must fall monotonically or the edge reads as a ring"
        );
    }
}

#[test]
fn a_splat_covers_the_middle_and_leaves_the_corners_alone() {
    let table = falloff_table();
    let mut surface = blank(40);
    splat(
        &mut surface,
        &Blob {
            x: 20.0,
            y: 20.0,
            radius: 10.0,
            color: OPAQUE_RED,
            seed: 1,
        },
        &table,
    );
    assert!(surface.get(20, 20).is_some_and(|p| p.a > 200));
    assert!(
        surface.get(0, 0).is_some_and(|p| p.a == 0),
        "a disc must not reach the corners of its own bounding box"
    );
}

#[test]
fn a_splat_outside_the_surface_draws_nothing_and_does_not_fault() {
    let table = falloff_table();
    let mut surface = blank(16);
    for (x, y) in [(-500.0, -500.0), (900.0, 900.0), (-40.0, 8.0)] {
        splat(
            &mut surface,
            &Blob {
                x,
                y,
                radius: 6.0,
                color: OPAQUE_RED,
                seed: 3,
            },
            &table,
        );
    }
    assert_eq!(covered(&surface), 0);
}

#[test]
fn a_blob_with_no_radius_or_no_alpha_draws_nothing() {
    let table = falloff_table();
    let mut surface = blank(20);
    splat(
        &mut surface,
        &Blob {
            x: 10.0,
            y: 10.0,
            radius: 0.0,
            color: OPAQUE_RED,
            seed: 1,
        },
        &table,
    );
    splat(
        &mut surface,
        &Blob {
            x: 10.0,
            y: 10.0,
            radius: 8.0,
            color: Color { a: 0, ..OPAQUE_RED },
            seed: 1,
        },
        &table,
    );
    assert_eq!(covered(&surface), 0);
}

#[test]
fn a_bigger_blob_covers_more() {
    let table = falloff_table();
    let mut small = blank(64);
    let mut large = blank(64);
    for (surface, radius) in [(&mut small, 6.0), (&mut large, 18.0)] {
        splat(
            surface,
            &Blob {
                x: 32.0,
                y: 32.0,
                radius,
                color: OPAQUE_RED,
                seed: 7,
            },
            &table,
        );
    }
    assert!(covered(&large) > covered(&small));
}

#[test]
fn the_ripple_is_the_same_every_time_for_a_given_blob() {
    // Fur that re-rippled each frame would boil, which reads as noise rather
    // than as a coat.
    // Bit patterns, because "the same every frame" is a bit-for-bit claim.
    let bits = |seed| ripple(seed).map(f64::to_bits);
    assert_eq!(bits(42), bits(42));
    assert_ne!(bits(42), bits(43));
}

#[test]
fn the_ripple_stays_near_the_nominal_rim() {
    for seed in [0u16, 1, 999, u16::MAX] {
        let rim = ripple(seed);
        assert_eq!(rim.len(), RIPPLE_SECTORS);
        for radius in rim {
            assert!(
                (0.7..=1.3).contains(&radius),
                "a ripple must texture the rim, not reshape the blob ({radius})"
            );
        }
    }
}

#[test]
fn a_shadow_is_wider_than_it_is_tall_and_soft_at_the_rim() {
    let mut surface = blank(64);
    shadow(&mut surface, 32.0, 32.0, 20.0, 10.0, 200);
    let centre = surface.get(32, 32).expect("in range").a;
    let edge = surface.get(50, 32).expect("in range").a;
    assert!(centre > edge, "a shadow fades towards its rim");
    assert!(
        surface.get(32, 12).is_some_and(|p| p.a == 0),
        "and is squashed: its vertical radius is the shorter one"
    );
}

#[test]
fn a_transparent_or_empty_shadow_draws_nothing() {
    let mut surface = blank(32);
    shadow(&mut surface, 16.0, 16.0, 8.0, 4.0, 0);
    shadow(&mut surface, 16.0, 16.0, 0.0, 4.0, 200);
    shadow(&mut surface, 16.0, 16.0, 8.0, 0.0, 200);
    assert_eq!(covered(&surface), 0);
}
