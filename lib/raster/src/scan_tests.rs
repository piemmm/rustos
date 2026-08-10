//! Unit tests for the shared scan converter, exercised through the surface
//! fill entry points it serves.

use alloc::vec;
use alloc::vec::Vec;

use tairix_util::mathf;

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
                centre + mathf::round_i32(radius * mathf::cos(angle)),
                centre + mathf::round_i32(radius * mathf::sin(angle)),
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
    // Exact coverage lands within a pixel of it, where a sample count could
    // only bracket it.
    let area = surface
        .pixels()
        .iter()
        .map(|pixel| u32::from(pixel.a))
        .sum::<u32>()
        / 255;
    assert!((2826..=2828).contains(&area), "filled area {area}");
}

/// `polygon` cut down to the side of an axis-aligned line it lies on.
///
/// One step of the classic convex-window clip: an edge that straddles the line
/// contributes the point where it meets it, and only points on the kept side
/// survive.
fn clip_axis(polygon: &[(f64, f64)], axis: usize, bound: f64, keep_above: bool) -> Vec<(f64, f64)> {
    let value = |point: (f64, f64)| if axis == 0 { point.0 } else { point.1 };
    let inside = |point: (f64, f64)| {
        if keep_above {
            value(point) >= bound
        } else {
            value(point) <= bound
        }
    };
    let mut kept = Vec::new();
    let Some(&last) = polygon.last() else {
        return kept;
    };
    let mut previous = last;
    for &point in polygon {
        if inside(previous) != inside(point) {
            let step = (bound - value(previous)) / (value(point) - value(previous));
            kept.push((
                previous.0 + step * (point.0 - previous.0),
                previous.1 + step * (point.1 - previous.1),
            ));
        }
        if inside(point) {
            kept.push(point);
        }
        previous = point;
    }
    kept
}

/// The signed area a closed `polygon` encloses, by the shoelace formula.
fn signed_area(polygon: &[(f64, f64)]) -> f64 {
    let Some(&last) = polygon.last() else {
        return 0.0;
    };
    let mut previous = last;
    let mut sum = 0.0;
    for &point in polygon {
        sum += previous.0 * point.1 - point.0 * previous.1;
        previous = point;
    }
    sum / 2.0
}

/// The alpha pixel `(x, y)` must take: the exact area `contours` cover inside
/// it, run through `rule`.
///
/// The area is derived independently of the converter — each contour is
/// clipped to the pixel's own square and measured with the shoelace formula —
/// because an oracle that shared the code it checks would prove nothing. Its
/// cost is every edge for every pixel, which is why it lives in a test over
/// small shapes.
fn reference_alpha(contours: &[Vec<(f64, f64)>], x: u32, y: u32, rule: FillRule) -> u8 {
    let (left, top) = (f64::from(x), f64::from(y));
    let mut signed = 0.0;
    for contour in contours {
        let clipped = clip_axis(contour, 0, left, true);
        let clipped = clip_axis(&clipped, 0, left + 1.0, false);
        let clipped = clip_axis(&clipped, 1, top, true);
        let clipped = clip_axis(&clipped, 1, top + 1.0, false);
        signed += signed_area(&clipped);
    }
    let covered = match rule {
        FillRule::NonZero => mathf::fabs(signed).min(1.0),
        FillRule::EvenOdd => {
            let wrapped = signed - 2.0 * mathf::floor(signed / 2.0);
            if wrapped > 1.0 {
                2.0 - wrapped
            } else {
                wrapped
            }
        }
    };
    u8::try_from(mathf::round_i32(covered * 255.0)).unwrap_or(u8::MAX)
}

/// `contours` in pixel coordinates, snapped to the sub-unit grid the converter
/// places vertices on, so the oracle measures the shape the converter was
/// actually given rather than the one before quantisation.
fn in_pixels(contours: &[Vec<(i32, i32)>], design: u32, size: u32) -> Vec<Vec<(f64, f64)>> {
    let place = |coordinate: i32| {
        let units = f64::from(coordinate) * f64::from(size) * 256.0 / f64::from(design.max(1));
        f64::from(mathf::round_i32(units)) / 256.0
    };
    contours
        .iter()
        .map(|contour| contour.iter().map(|&(x, y)| (place(x), place(y))).collect())
        .collect()
}

/// Assert a filled shape takes the exact area of every pixel it covers.
///
/// One alpha step of slack: the converter rounds an edge's crossing of a
/// column boundary to the nearest sub-unit, which can move a pixel's area by
/// up to a 256th of it.
#[track_caller]
fn assert_matches_exact_area(contours: &[Vec<(i32, i32)>], design: u32, size: u32, rule: FillRule) {
    let mut surface = Surface::new(size, size).expect("allocates");
    surface.fill_contours(contours, design, rule, &Paint::Solid(RED));
    let placed = in_pixels(contours, design, size);

    for y in 0..size {
        for x in 0..size {
            let want = reference_alpha(&placed, x, y, rule);
            let got = surface.get(x, y).expect("in bounds").a;
            assert!(
                got.abs_diff(want) <= 1,
                "{rule:?} pixel ({x}, {y}): {got} is not the exact area {want}"
            );
        }
    }
}

#[test]
fn the_scan_converter_paints_the_exact_area_a_shape_covers() {
    // Sub-pixel geometry and nested contours of either winding must all come
    // out as the area they genuinely cover, not as a count of sample points
    // that happened to land inside.
    let triangle = vec![vec![(3, 1), (29, 7), (11, 30)]];
    let nested = vec![square(2, 28), reversed_square(9, 13)];
    let same_winding = vec![square(2, 28), square(9, 13)];
    for rule in [FillRule::NonZero, FillRule::EvenOdd] {
        assert_matches_exact_area(&triangle, 32, 16, rule);
        assert_matches_exact_area(&nested, 32, 32, rule);
        assert_matches_exact_area(&same_winding, 32, 32, rule);
    }
}

#[test]
fn a_fractionally_placed_edge_takes_its_exact_share_of_a_pixel() {
    // A rectangle spanning x = 2.34375 to x = 5.65625 pixels, so each end
    // pixel is 21/32 covered. That is not a multiple of a sixteenth, which is
    // all a 4×4 sample grid can say; failing to express it is what smears a
    // small shape. 21/32 of full alpha is 167.
    let mut surface = Surface::new(8, 8).expect("allocates");
    let rect = vec![vec![(75, 0), (181, 0), (181, 256), (75, 256)]];
    surface.fill_contours(&rect, 256, FillRule::NonZero, &Paint::Solid(RED));

    let alpha = |x: u32| surface.get(x, 4).expect("in bounds").a;
    assert_eq!(alpha(1), 0, "clear of the shape");
    assert_eq!(alpha(2), 167, "21/32 of the pixel");
    assert_eq!(alpha(3), 255, "wholly inside");
    assert_eq!(alpha(4), 255, "wholly inside");
    assert_eq!(alpha(5), 167, "21/32 of the pixel");
    assert_eq!(alpha(6), 0, "clear of the shape");
}

#[test]
fn a_shape_symmetric_about_its_centre_rasterises_symmetrically() {
    // The visible failure of point sampling: a bar's two edges are the same
    // distance from the pixel grid but round to different sample counts, so
    // one side of an icon comes out harder than the other. Exact area cannot
    // do that — the two shares are equal, so the two alphas are.
    let mut surface = Surface::new(24, 24).expect("allocates");
    // Deliberately off-grid on both axes: 1.7 to 22.3 pixels.
    let bar = vec![vec![(17, 43), (223, 43), (223, 197), (17, 197)]];
    surface.fill_contours(&bar, 240, FillRule::EvenOdd, &Paint::Solid(RED));

    for y in 0..24 {
        for x in 0..24 {
            let here = surface.get(x, y).expect("in bounds");
            assert_eq!(
                here,
                surface.get(23 - x, y).expect("in bounds"),
                "({x}, {y})"
            );
            assert_eq!(
                here,
                surface.get(x, 23 - y).expect("in bounds"),
                "({x}, {y})"
            );
        }
    }
}

#[test]
fn the_two_rules_disagree_where_a_contour_crosses_itself() {
    // A pentagram's core is wound twice: non-zero fills it, even-odd hollows
    // it. Its exact alpha at the five crossings is not pinned — a pixel that
    // holds regions of two different winding depths is the one place area
    // coverage approximates, since a pixel's area carries no record of which
    // part of it each depth occupied.
    let star: Vec<Vec<(i32, i32)>> = vec![vec![(16, 1), (23, 29), (1, 11), (31, 11), (9, 29)]];
    let mut surface = Surface::new(24, 24).expect("allocates");

    surface.fill_contours(&star, 32, FillRule::NonZero, &Paint::Solid(RED));
    assert_eq!(surface.get(12, 12), Some(RED.premultiply()), "core filled");

    surface.fill(Color::TRANSPARENT);
    surface.fill_contours(&star, 32, FillRule::EvenOdd, &Paint::Solid(RED));
    assert_eq!(
        surface.get(12, 12),
        Some(Pixel::TRANSPARENT),
        "core hollowed"
    );
    // Both rules agree inside a point of the star, which one winding reaches.
    assert_eq!(surface.get(12, 5), Some(RED.premultiply()), "a point fills");
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
