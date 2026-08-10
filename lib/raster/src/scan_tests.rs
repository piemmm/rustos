//! Unit tests for the shared scan converter, exercised through the surface
//! fill entry points it serves.

use alloc::vec;
use alloc::vec::Vec;

use super::FillRule;
use crate::affine::Affine;
use crate::color::{Color, Pixel};
use crate::paint::{Gradient, GradientKind, GradientStop, Paint, SpreadMethod};
use crate::surface::Surface;

const RED: Color = Color::rgb(255, 0, 0);
const BLACK: Color = Color::rgb(0, 0, 0);
const WHITE: Color = Color::rgb(255, 255, 255);

/// A `size`×`size` square with its top-left corner at `(at, at)`, wound
/// clockwise.
fn square(at: i32, size: i32) -> Vec<(i32, i32)> {
    vec![
        (at, at),
        (at + size, at),
        (at + size, at + size),
        (at, at + size),
    ]
}

/// The same square wound the other way, which is what tells the non-zero rule
/// a contour is a hole.
fn reversed_square(at: i32, size: i32) -> Vec<(i32, i32)> {
    let mut points = square(at, size);
    points.reverse();
    points
}

/// A regular `points`-gon inscribed in a circle of `radius` about
/// `(centre, centre)` — a stand-in for a flattened curve.
fn circle(centre: i32, radius: i32, points: u32) -> Vec<(i32, i32)> {
    (0..points)
        .map(|step| {
            let angle = 2.0 * core::f64::consts::PI * f64::from(step) / f64::from(points);
            let radius = f64::from(radius);
            (
                centre + tairix_util::mathf::round_i32(radius * tairix_util::mathf::cos(angle)),
                centre + tairix_util::mathf::round_i32(radius * tairix_util::mathf::sin(angle)),
            )
        })
        .collect()
}

#[test]
fn a_square_with_a_hole_is_hollow_under_even_odd() {
    let mut surface = Surface::new(16, 16).expect("allocates");
    let contours = vec![square(0, 16), square(4, 8)];
    surface.fill_contours(&contours, 16, FillRule::EvenOdd, &Paint::Solid(RED));

    assert_eq!(surface.get(1, 1), Some(RED.premultiply()), "the ring fills");
    assert_eq!(
        surface.get(8, 8),
        Some(Pixel::TRANSPARENT),
        "the hole stays empty"
    );
}

#[test]
fn a_hole_wound_the_same_way_fills_solid_under_non_zero() {
    // Two contours winding the same way accumulate to two, never back to
    // zero, so the non-zero rule sees one solid shape.
    let mut surface = Surface::new(16, 16).expect("allocates");
    let contours = vec![square(0, 16), square(4, 8)];
    surface.fill_contours(&contours, 16, FillRule::NonZero, &Paint::Solid(RED));

    assert!(
        surface.pixels().iter().all(|p| *p == RED.premultiply()),
        "every pixel must be filled"
    );
}

#[test]
fn a_hole_wound_the_other_way_is_hollow_under_non_zero() {
    let mut surface = Surface::new(16, 16).expect("allocates");
    let contours = vec![square(0, 16), reversed_square(4, 8)];
    surface.fill_contours(&contours, 16, FillRule::NonZero, &Paint::Solid(RED));

    assert_eq!(surface.get(1, 1), Some(RED.premultiply()));
    assert_eq!(surface.get(8, 8), Some(Pixel::TRANSPARENT));
    // The hole's own edge is where the two windings meet: it must be a clean
    // boundary, not a doubled fringe.
    assert_eq!(surface.get(3, 8), Some(RED.premultiply()));
    assert_eq!(surface.get(4, 8), Some(Pixel::TRANSPARENT));
}

#[test]
fn two_disjoint_contours_both_fill() {
    let mut surface = Surface::new(16, 16).expect("allocates");
    let contours = vec![square(1, 4), square(10, 4)];
    for rule in [FillRule::NonZero, FillRule::EvenOdd] {
        surface.fill(Color::TRANSPARENT);
        surface.fill_contours(&contours, 16, rule, &Paint::Solid(RED));
        assert_eq!(surface.get(2, 2), Some(RED.premultiply()), "{rule:?}");
        assert_eq!(surface.get(11, 11), Some(RED.premultiply()), "{rule:?}");
        assert_eq!(surface.get(7, 7), Some(Pixel::TRANSPARENT), "{rule:?}");
    }
}

#[test]
fn an_empty_contour_list_or_a_degenerate_contour_draws_nothing() {
    let mut surface = Surface::new(8, 8).expect("allocates");
    surface.fill_contours(&[], 8, FillRule::NonZero, &Paint::Solid(RED));
    let two_points = vec![vec![(0, 0), (8, 8)]];
    surface.fill_contours(&two_points, 8, FillRule::NonZero, &Paint::Solid(RED));
    let empty = vec![Vec::new()];
    surface.fill_contours(&empty, 8, FillRule::NonZero, &Paint::Solid(RED));
    assert!(surface.pixels().iter().all(|p| *p == Pixel::TRANSPARENT));
}

#[test]
fn a_degenerate_contour_beside_a_real_one_does_not_disturb_it() {
    let mut surface = Surface::new(8, 8).expect("allocates");
    let contours = vec![vec![(0, 0), (8, 0)], square(0, 8)];
    surface.fill_contours(&contours, 8, FillRule::NonZero, &Paint::Solid(RED));
    assert!(surface.pixels().iter().all(|p| *p == RED.premultiply()));
}

#[test]
fn a_zero_design_grid_is_read_as_one() {
    let mut surface = Surface::new(4, 4).expect("allocates");
    let contours = vec![square(0, 1)];
    surface.fill_contours(&contours, 0, FillRule::NonZero, &Paint::Solid(RED));
    assert!(surface.pixels().iter().all(|p| *p == RED.premultiply()));
}

#[test]
fn a_horizontal_edge_produces_no_spurious_crossing() {
    // A contour that runs flat along a sample row would, if its horizontal
    // edges were counted, toggle the inside state there and leave a bright or
    // missing line across the shape.
    let mut surface = Surface::new(8, 8).expect("allocates");
    let stepped = vec![vec![(0, 0), (8, 0), (8, 4), (4, 4), (4, 8), (0, 8)]];
    surface.fill_contours(&stepped, 8, FillRule::NonZero, &Paint::Solid(RED));
    for y in 0..8 {
        for x in 0..8 {
            let inside = y < 4 || x < 4;
            let want = if inside {
                RED.premultiply()
            } else {
                Pixel::TRANSPARENT
            };
            assert_eq!(surface.get(x, y), Some(want), "pixel ({x}, {y})");
        }
    }
}

#[test]
fn a_diagonal_edge_is_anti_aliased() {
    let mut surface = Surface::new(8, 8).expect("allocates");
    let triangle = vec![vec![(0, 0), (8, 0), (0, 8)]];
    surface.fill_contours(&triangle, 8, FillRule::NonZero, &Paint::Solid(RED));

    let edge = surface.get(3, 4).expect("in bounds");
    assert!(
        edge.a > 0 && edge.a < 255,
        "the diagonal must be partially covered: {edge:?}"
    );
    assert_eq!(surface.get(0, 0), Some(RED.premultiply()));
    assert_eq!(surface.get(7, 7), Some(Pixel::TRANSPARENT));
}

#[test]
fn a_gradient_paints_its_ends_and_midpoint() {
    // The ramp is authored in the contours' own design coordinates, so the
    // transform into canonical space is what places it across the shape.
    let mut surface = Surface::new(16, 1).expect("allocates");
    let contours = vec![square(0, 16)];
    let paint = Paint::Gradient(Gradient {
        kind: GradientKind::Linear,
        stops: vec![
            GradientStop {
                offset: 0.0,
                color: BLACK,
            },
            GradientStop {
                offset: 1.0,
                color: WHITE,
            },
        ],
        spread: SpreadMethod::Pad,
        to_gradient: Affine::scale(1.0 / 16.0, 1.0),
    });
    surface.fill_contours(&contours, 16, FillRule::NonZero, &paint);

    // Pixel centres sit half a design unit in, so the first and last pixels
    // are a half-step inside the ramp rather than exactly at its ends.
    let first = surface.get(0, 0).expect("in bounds");
    let middle = surface.get(8, 0).expect("in bounds");
    let last = surface.get(15, 0).expect("in bounds");
    assert_eq!(first.a, 255);
    assert_eq!(first.r, 8, "half a design unit into the ramp");
    assert_eq!(middle.r, 135, "8.5 of 16 design units along");
    assert_eq!(last.r, 247, "half a design unit short of the end");
}

#[test]
fn a_gradient_is_sampled_per_pixel_in_both_axes() {
    let mut surface = Surface::new(8, 8).expect("allocates");
    let contours = vec![square(0, 8)];
    let paint = Paint::Gradient(Gradient {
        kind: GradientKind::Linear,
        stops: vec![
            GradientStop {
                offset: 0.0,
                color: BLACK,
            },
            GradientStop {
                offset: 1.0,
                color: WHITE,
            },
        ],
        spread: SpreadMethod::Pad,
        // A ramp running down the shape rather than across it: the pixel
        // centre's y must reach the paint, not just its x.
        to_gradient: Affine::rotate_degrees(-90.0).then(Affine::scale(1.0 / 8.0, 1.0)),
    });
    surface.fill_contours(&contours, 8, FillRule::NonZero, &paint);

    let top = surface.get(4, 0).expect("in bounds");
    let bottom = surface.get(4, 7).expect("in bounds");
    assert!(top.r < bottom.r, "the ramp must run downward: {top:?}");
    // Every pixel of a row shares the row's colour.
    assert_eq!(surface.get(0, 3), surface.get(7, 3));
}

#[test]
fn a_gradient_with_no_stops_paints_nothing() {
    let mut surface = Surface::new(4, 4).expect("allocates");
    let paint = Paint::Gradient(Gradient {
        kind: GradientKind::Linear,
        stops: Vec::new(),
        spread: SpreadMethod::Pad,
        to_gradient: Affine::IDENTITY,
    });
    surface.fill_contours(&[square(0, 4)], 4, FillRule::NonZero, &paint);
    assert!(surface.pixels().iter().all(|p| *p == Pixel::TRANSPARENT));
}

#[test]
fn a_contour_of_several_thousand_points_fills_correctly() {
    // A flattened curve is thousands of short edges. The scan converter costs
    // the edges once per sample row rather than once per sub-sample, which is
    // what makes this shape drawable at all.
    let mut surface = Surface::new(64, 64).expect("allocates");
    // A design grid finer than the surface, so the vertices land between
    // pixel boundaries and the rim is a genuine anti-aliased curve rather than
    // a rectilinear staircase.
    let contours = vec![circle(512, 480, 4096)];
    surface.fill_contours(&contours, 1024, FillRule::NonZero, &Paint::Solid(RED));

    assert_eq!(surface.get(32, 32), Some(RED.premultiply()), "the centre");
    assert_eq!(
        surface.get(32, 5),
        Some(RED.premultiply()),
        "inside the top"
    );
    assert_eq!(surface.get(0, 0), Some(Pixel::TRANSPARENT), "the corner");
    assert_eq!(
        surface.get(63, 63),
        Some(Pixel::TRANSPARENT),
        "the far corner"
    );

    let partial = surface
        .pixels()
        .iter()
        .filter(|pixel| pixel.a > 0 && pixel.a < 255)
        .count();
    assert!(
        partial > 32,
        "the rim must be anti-aliased: {partial} pixels"
    );

    // Summing the coverage measures the filled area, so a missing span or a
    // leaked one shows up as an area the circle's own πr² does not explain.
    let area = surface
        .pixels()
        .iter()
        .map(|pixel| u32::from(pixel.a))
        .sum::<u32>()
        / 255;
    assert!((2790..=2870).contains(&area), "filled area {area}");
}

/// Whether one sample point is inside `contours` under `rule`, decided the
/// obvious way: cast a ray in `+x` and account for every edge it crosses.
///
/// This is deliberately a second, independent derivation of the answer rather
/// than a call into the scan converter — an oracle is worthless if it shares
/// the code it is checking. Its cost (every edge for every sub-sample of every
/// pixel) is exactly what the scan converter exists to avoid, which is why it
/// lives in a test over small shapes.
fn reference_inside(contours: &[Vec<(i64, i64)>], at: (i64, i64), rule: FillRule) -> bool {
    let (x, y) = at;
    let mut winding = 0;
    for contour in contours {
        if contour.len() < 3 {
            continue;
        }
        for (index, &(x1, y1)) in contour.iter().enumerate() {
            let (x2, y2) = contour[(index + 1) % contour.len()];
            if (y1 > y) == (y2 > y) {
                continue;
            }
            let side = (x - x1) * (y2 - y1);
            let across = (x2 - x1) * (y - y1);
            let to_the_right = if y2 > y1 {
                side < across
            } else {
                side > across
            };
            if to_the_right {
                winding = match rule {
                    FillRule::NonZero if y2 > y1 => winding + 1,
                    FillRule::NonZero => winding - 1,
                    FillRule::EvenOdd => winding + 1,
                };
            }
        }
    }
    match rule {
        FillRule::NonZero => winding != 0,
        FillRule::EvenOdd => winding % 2 != 0,
    }
}

/// Assert a filled shape matches the reference oracle pixel for pixel.
#[track_caller]
fn assert_matches_reference(contours: &[Vec<(i32, i32)>], design: u32, size: u32, rule: FillRule) {
    let mut surface = Surface::new(size, size).expect("allocates");
    surface.fill_contours(contours, design, rule, &Paint::Solid(RED));

    let denominator = i64::from(design.max(1));
    let scaled: Vec<Vec<(i64, i64)>> = contours
        .iter()
        .map(|contour| {
            contour
                .iter()
                .map(|&(x, y)| {
                    (
                        i64::from(x) * i64::from(size) * 8 / denominator,
                        i64::from(y) * i64::from(size) * 8 / denominator,
                    )
                })
                .collect()
        })
        .collect();

    for y in 0..size {
        for x in 0..size {
            let mut hits = 0;
            for row in 0..4 {
                for column in 0..4 {
                    let at = (
                        i64::from(x) * 8 + 2 * i64::from(column) + 1,
                        i64::from(y) * 8 + 2 * i64::from(row) + 1,
                    );
                    if reference_inside(&scaled, at, rule) {
                        hits += 1;
                    }
                }
            }
            let want = u8::try_from(255 * hits / 16).unwrap_or(u8::MAX);
            let got = surface.get(x, y).expect("in bounds").a;
            assert_eq!(got, want, "{rule:?} pixel ({x}, {y})");
        }
    }
}

#[test]
fn the_scan_converter_matches_the_per_sample_reference() {
    // Sub-pixel geometry, nested contours of both windings, and a
    // self-intersecting outline — where the two rules genuinely disagree —
    // must all come out exactly as probing every sub-sample would.
    let triangle = vec![vec![(3, 1), (29, 7), (11, 30)]];
    let nested = vec![square(2, 28), reversed_square(9, 13)];
    let same_winding = vec![square(2, 28), square(9, 13)];
    let star: Vec<Vec<(i32, i32)>> = vec![vec![(16, 1), (23, 29), (1, 11), (31, 11), (9, 29)]];
    let overlapping = vec![
        vec![(1, 1), (25, 5), (7, 22)],
        vec![(20, 6), (30, 28), (5, 25)],
    ];
    for rule in [FillRule::NonZero, FillRule::EvenOdd] {
        assert_matches_reference(&triangle, 32, 16, rule);
        assert_matches_reference(&nested, 32, 32, rule);
        assert_matches_reference(&same_winding, 32, 32, rule);
        assert_matches_reference(&star, 32, 24, rule);
        assert_matches_reference(&overlapping, 32, 20, rule);
    }
}

#[test]
fn wild_coordinates_neither_panic_nor_paint_outside_the_surface() {
    let mut surface = Surface::new(8, 8).expect("allocates");
    let extremes = vec![
        vec![
            (i32::MIN, i32::MIN),
            (i32::MAX, i32::MIN),
            (i32::MAX, i32::MAX),
            (i32::MIN, i32::MAX),
        ],
        vec![(i32::MIN, 0), (0, i32::MAX), (i32::MAX, i32::MIN)],
        square(-3, i32::MAX),
    ];
    for rule in [FillRule::NonZero, FillRule::EvenOdd] {
        surface.fill_contours(&extremes, 1, rule, &Paint::Solid(RED));
        surface.fill_contours(&extremes, u32::MAX, rule, &Paint::Solid(RED));
    }
    // Nothing to assert about the pixels beyond that the calls returned; the
    // point is that no arithmetic overflowed and no index went out of bounds.
    assert_eq!(surface.width(), 8);
}
