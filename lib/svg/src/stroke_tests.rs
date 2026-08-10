//! Unit tests for stroking.

use alloc::vec::Vec;

use tairix_util::mathf::sqrt;

use crate::error::SvgError;
use crate::geom::{LineCap, LineJoin, Point, StrokeStyle, SubPath};

use super::{signed_area, stroke_outline};

/// A budget far larger than any shape here needs, so only the budget test
/// itself reaches it.
const BUDGET: usize = 100_000;

/// The arc accuracy the tests ask for, in the same units as their
/// coordinates.
const TOL: f64 = 0.01;

/// A solid stroke of `width`, with everything else at SVG's initial values.
fn plain(width: f64) -> StrokeStyle {
    StrokeStyle {
        width,
        ..StrokeStyle::default()
    }
}

#[track_caller]
fn close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() <= 1e-6,
        "expected {expected}, got {actual}"
    );
}

/// The bounds of everything the stroke covers.
fn bounds(pieces: &[SubPath]) -> (Point, Point) {
    let mut min = (f64::MAX, f64::MAX);
    let mut max = (f64::MIN, f64::MIN);
    for point in pieces.iter().flat_map(|piece| piece.points.iter()) {
        min = (min.0.min(point.0), min.1.min(point.1));
        max = (max.0.max(point.0), max.1.max(point.1));
    }
    (min, max)
}

/// Whether any emitted point sits within a whisker of `wanted`.
fn has_point(pieces: &[SubPath], wanted: Point) -> bool {
    pieces
        .iter()
        .flat_map(|piece| piece.points.iter())
        .any(|point| (point.0 - wanted.0).abs() < 1e-6 && (point.1 - wanted.1).abs() < 1e-6)
}

/// The distance from `point` to the polyline through `line`.
fn distance_to_polyline(point: Point, line: &[Point]) -> f64 {
    line.windows(2)
        .map(|pair| {
            let (a, b) = (pair[0], pair[1]);
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let length2 = dx * dx + dy * dy;
            let t = if length2 == 0.0 {
                0.0
            } else {
                (((point.0 - a.0) * dx + (point.1 - a.1) * dy) / length2).clamp(0.0, 1.0)
            };
            let (nx, ny) = (point.0 - (a.0 + t * dx), point.1 - (a.1 + t * dy));
            sqrt(nx * nx + ny * ny)
        })
        .fold(f64::MAX, f64::min)
}

// --- the basic shape ------------------------------------------------------

#[test]
fn a_segment_strokes_a_rectangle_of_its_width_and_length() {
    let line = SubPath::open(alloc::vec![(0.0, 0.0), (10.0, 0.0)]);
    let pieces = stroke_outline(&[line], &plain(2.0), TOL, BUDGET).expect("a stroke");
    assert_eq!(pieces.len(), 1);
    assert_eq!(pieces[0].points.len(), 4);
    let (min, max) = bounds(&pieces);
    close(min.0, 0.0);
    close(max.0, 10.0);
    close(min.1, -1.0);
    close(max.1, 1.0);
}

/// The whole design rests on this: the pieces are unioned by the non-zero
/// rule, which only works if they all wind the same way.
#[test]
fn every_piece_is_wound_the_same_way() {
    let shapes = [
        SubPath::open(alloc::vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)]),
        SubPath::closed(alloc::vec![(0.0, 0.0), (20.0, 0.0), (10.0, 15.0)]),
        SubPath::closed(alloc::vec![(0.0, 0.0), (10.0, 15.0), (20.0, 0.0)]),
        SubPath::open(alloc::vec![(5.0, 5.0), (5.0, 5.0)]),
    ];
    for join in [LineJoin::Miter, LineJoin::Round, LineJoin::Bevel] {
        for cap in [LineCap::Butt, LineCap::Round, LineCap::Square] {
            let style = StrokeStyle {
                width: 3.0,
                cap,
                join,
                ..StrokeStyle::default()
            };
            let pieces = stroke_outline(&shapes, &style, TOL, BUDGET).expect("a stroke");
            for piece in &pieces {
                assert!(
                    signed_area(&piece.points) <= 0.0,
                    "a piece wound the other way under {join:?}/{cap:?}"
                );
            }
        }
    }
}

/// Whatever the joins and caps do, no part of a stroke may stray further
/// from the line than half its width.
#[test]
fn no_stroke_point_strays_past_half_the_width() {
    let line = alloc::vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 12.0)];
    for join in [LineJoin::Round, LineJoin::Bevel] {
        for cap in [LineCap::Butt, LineCap::Round] {
            let style = StrokeStyle {
                width: 4.0,
                cap,
                join,
                ..StrokeStyle::default()
            };
            let pieces = stroke_outline(&[SubPath::open(line.clone())], &style, TOL, BUDGET)
                .expect("a stroke");
            for point in pieces.iter().flat_map(|piece| piece.points.iter()) {
                let away = distance_to_polyline(*point, &line);
                assert!(away <= 2.0 + TOL, "a {join:?}/{cap:?} point strayed {away}");
            }
        }
    }
}

// --- joins ----------------------------------------------------------------

/// A right angle: the miter reaches the outer corner, the bevel cuts it off,
/// and the round join stays inside the half-width disc.
#[test]
fn each_join_shapes_the_corner_it_names() {
    let corner = alloc::vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)];
    let styled = |join| StrokeStyle {
        width: 2.0,
        join,
        ..StrokeStyle::default()
    };

    let miter = stroke_outline(
        &[SubPath::open(corner.clone())],
        &styled(LineJoin::Miter),
        TOL,
        BUDGET,
    )
    .expect("a miter");
    assert!(has_point(&miter, (11.0, -1.0)), "the miter apex is missing");

    let bevel = stroke_outline(
        &[SubPath::open(corner.clone())],
        &styled(LineJoin::Bevel),
        TOL,
        BUDGET,
    )
    .expect("a bevel");
    assert!(!has_point(&bevel, (11.0, -1.0)), "a bevel has no apex");

    let round = stroke_outline(
        &[SubPath::open(corner)],
        &styled(LineJoin::Round),
        TOL,
        BUDGET,
    )
    .expect("a round join");
    for point in round.iter().flat_map(|piece| piece.points.iter()) {
        let (dx, dy) = (point.0 - 10.0, point.1);
        let from_corner = sqrt(dx * dx + dy * dy);
        assert!(
            from_corner <= 1.0 + TOL || point.0 < 9.99 || point.1 > 0.01,
            "a round join point escaped its disc"
        );
    }
}

/// A near reversal would give a miter an unbounded spike, so beyond the
/// limit SVG cuts it square.
#[test]
fn the_miter_limit_degrades_a_spike_to_a_bevel() {
    let spike = alloc::vec![(0.0, 0.0), (10.0, 0.0), (0.0, 0.2)];
    let style = StrokeStyle {
        width: 2.0,
        join: LineJoin::Miter,
        miter_limit: 4.0,
        ..StrokeStyle::default()
    };
    let pieces = stroke_outline(&[SubPath::open(spike)], &style, TOL, BUDGET).expect("a stroke");
    for point in pieces.iter().flat_map(|piece| piece.points.iter()) {
        let dx = point.0 - 10.0;
        let from_corner = sqrt(dx * dx + point.1 * point.1);
        assert!(
            from_corner <= 4.0 * 1.0 + 1e-6 || point.0 < 9.0,
            "the miter spike reached {from_corner} past its limit"
        );
    }
}

/// A raised limit lets the same corner keep its spike, which is what proves
/// the limit is what cut it.
#[test]
fn a_raised_miter_limit_keeps_the_spike() {
    let spike = alloc::vec![(0.0, 0.0), (10.0, 0.0), (0.0, 0.2)];
    let style = StrokeStyle {
        width: 2.0,
        join: LineJoin::Miter,
        miter_limit: 1000.0,
        ..StrokeStyle::default()
    };
    let pieces = stroke_outline(&[SubPath::open(spike)], &style, TOL, BUDGET).expect("a stroke");
    let (_, max) = bounds(&pieces);
    assert!(max.0 > 20.0, "the spike should reach well past the corner");
}

// --- caps -----------------------------------------------------------------

#[test]
fn each_cap_finishes_the_end_it_names() {
    let line = alloc::vec![(0.0, 0.0), (10.0, 0.0)];
    let styled = |cap| StrokeStyle {
        width: 2.0,
        cap,
        ..StrokeStyle::default()
    };

    let butt = stroke_outline(
        &[SubPath::open(line.clone())],
        &styled(LineCap::Butt),
        TOL,
        BUDGET,
    )
    .expect("a butt cap");
    let (min, max) = bounds(&butt);
    close(min.0, 0.0);
    close(max.0, 10.0);

    let square = stroke_outline(
        &[SubPath::open(line.clone())],
        &styled(LineCap::Square),
        TOL,
        BUDGET,
    )
    .expect("a square cap");
    let (min, max) = bounds(&square);
    close(min.0, -1.0);
    close(max.0, 11.0);

    let round = stroke_outline(&[SubPath::open(line)], &styled(LineCap::Round), TOL, BUDGET)
        .expect("a round cap");
    let (min, max) = bounds(&round);
    assert!(min.0 >= -1.0 - TOL && min.0 <= -1.0 + TOL);
    assert!(max.0 <= 11.0 + TOL && max.0 >= 11.0 - TOL);
}

#[test]
fn a_closed_contour_has_no_caps() {
    let triangle = SubPath::closed(alloc::vec![(0.0, 0.0), (10.0, 0.0), (5.0, 8.0)]);
    let style = StrokeStyle {
        width: 2.0,
        cap: LineCap::Square,
        join: LineJoin::Bevel,
        ..StrokeStyle::default()
    };
    let pieces = stroke_outline(&[triangle], &style, TOL, BUDGET).expect("a stroke");
    // Three segment rectangles and three corner wedges, and nothing else: a
    // cap anywhere would be a fourth kind of piece.
    assert_eq!(pieces.len(), 6);
}

#[test]
fn a_single_point_strokes_a_dot_only_under_a_finishing_cap() {
    let dot = SubPath::open(alloc::vec![(5.0, 5.0)]);
    let styled = |cap| StrokeStyle {
        width: 2.0,
        cap,
        ..StrokeStyle::default()
    };

    assert!(stroke_outline(
        core::slice::from_ref(&dot),
        &styled(LineCap::Butt),
        TOL,
        BUDGET
    )
    .expect("a stroke")
    .is_empty());

    let square = stroke_outline(
        core::slice::from_ref(&dot),
        &styled(LineCap::Square),
        TOL,
        BUDGET,
    )
    .expect("a stroke");
    let (min, max) = bounds(&square);
    close(min.0, 4.0);
    close(max.0, 6.0);

    let round = stroke_outline(&[dot], &styled(LineCap::Round), TOL, BUDGET).expect("a stroke");
    assert_eq!(round.len(), 1);
    for point in &round[0].points {
        let (dx, dy) = (point.0 - 5.0, point.1 - 5.0);
        let radius = sqrt(dx * dx + dy * dy);
        close(radius, 1.0);
    }
}

// --- widths ---------------------------------------------------------------

#[test]
fn a_width_that_is_not_a_positive_number_strokes_nothing() {
    let line = SubPath::open(alloc::vec![(0.0, 0.0), (10.0, 0.0)]);
    for width in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert_eq!(
            stroke_outline(core::slice::from_ref(&line), &plain(width), TOL, BUDGET),
            Ok(Vec::new()),
            "width {width} should stroke nothing"
        );
    }
}

// --- dashes ---------------------------------------------------------------

#[test]
fn a_dash_pattern_breaks_the_line_into_the_expected_runs() {
    let line = SubPath::open(alloc::vec![(0.0, 0.0), (100.0, 0.0)]);
    let style = StrokeStyle {
        width: 2.0,
        dashes: alloc::vec![10.0, 10.0],
        ..StrokeStyle::default()
    };
    let pieces = stroke_outline(&[line], &style, TOL, BUDGET).expect("a dashed stroke");
    assert_eq!(pieces.len(), 5);
    let (min, max) = bounds(&pieces);
    close(min.0, 0.0);
    close(max.0, 90.0);
}

#[test]
fn a_dash_offset_shifts_the_pattern() {
    let line = SubPath::open(alloc::vec![(0.0, 0.0), (100.0, 0.0)]);
    let style = StrokeStyle {
        width: 2.0,
        dashes: alloc::vec![10.0, 10.0],
        dash_offset: 10.0,
        ..StrokeStyle::default()
    };
    let pieces = stroke_outline(&[line], &style, TOL, BUDGET).expect("a dashed stroke");
    let (min, _) = bounds(&pieces);
    close(min.0, 10.0);
}

#[test]
fn a_negative_dash_offset_wraps_into_the_pattern() {
    let line = SubPath::open(alloc::vec![(0.0, 0.0), (100.0, 0.0)]);
    let style = StrokeStyle {
        width: 2.0,
        dashes: alloc::vec![10.0, 10.0],
        dash_offset: -10.0,
        ..StrokeStyle::default()
    };
    let pieces = stroke_outline(&[line], &style, TOL, BUDGET).expect("a dashed stroke");
    let (min, _) = bounds(&pieces);
    close(min.0, 10.0);
}

/// An odd-length pattern repeats to make it even, so `[10]` is `[10, 10]`.
#[test]
fn an_odd_dash_pattern_is_doubled() {
    let line = SubPath::open(alloc::vec![(0.0, 0.0), (100.0, 0.0)]);
    let odd = StrokeStyle {
        width: 2.0,
        dashes: alloc::vec![10.0],
        ..StrokeStyle::default()
    };
    let even = StrokeStyle {
        dashes: alloc::vec![10.0, 10.0],
        ..odd.clone()
    };
    let from_odd =
        stroke_outline(core::slice::from_ref(&line), &odd, TOL, BUDGET).expect("a stroke");
    let from_even = stroke_outline(&[line], &even, TOL, BUDGET).expect("a stroke");
    assert_eq!(from_odd.len(), from_even.len());
}

#[test]
fn a_degenerate_dash_pattern_draws_solid() {
    let line = SubPath::open(alloc::vec![(0.0, 0.0), (100.0, 0.0)]);
    for dashes in [
        alloc::vec![0.0, 0.0],
        alloc::vec![-5.0, 5.0],
        alloc::vec![f64::NAN, 5.0],
    ] {
        let style = StrokeStyle {
            width: 2.0,
            dashes,
            ..StrokeStyle::default()
        };
        let pieces =
            stroke_outline(core::slice::from_ref(&line), &style, TOL, BUDGET).expect("a stroke");
        assert_eq!(pieces.len(), 1, "a degenerate pattern should draw solid");
    }
}

/// A dash that runs over a closed contour's seam is one run, not two with
/// their caps facing each other across the join.
#[test]
fn a_dash_spanning_a_seam_is_one_run() {
    let square = SubPath::closed(alloc::vec![
        (0.0, 0.0),
        (30.0, 0.0),
        (30.0, 30.0),
        (0.0, 30.0),
    ]);
    let style = StrokeStyle {
        width: 2.0,
        cap: LineCap::Butt,
        dashes: alloc::vec![20.0, 20.0],
        ..StrokeStyle::default()
    };
    let joined = stroke_outline(&[square], &style, TOL, BUDGET).expect("a dashed stroke");
    // The perimeter is 120, so the pattern lays down three 20-unit dashes;
    // the last wraps onto the first rather than ending at the seam.
    let runs = joined
        .iter()
        .filter(|piece| piece.points.len() == 4)
        .count();
    assert!(runs >= 3, "expected the dashes to survive the seam");
}

/// A dash pattern fine enough to cut a long line into thousands of pieces
/// must fail closed rather than allocate without end.
#[test]
fn an_unaffordable_dash_pattern_is_refused() {
    let line = SubPath::open(alloc::vec![(0.0, 0.0), (1.0e6, 0.0)]);
    let style = StrokeStyle {
        width: 1.0,
        dashes: alloc::vec![0.001, 0.001],
        ..StrokeStyle::default()
    };
    assert_eq!(
        stroke_outline(&[line], &style, TOL, 1000),
        Err(SvgError::TooComplex)
    );
}

#[test]
fn exceeding_the_point_budget_is_refused() {
    let line = SubPath::open(alloc::vec![(0.0, 0.0), (10.0, 0.0)]);
    assert_eq!(
        stroke_outline(&[line], &plain(2.0), TOL, 3),
        Err(SvgError::TooComplex)
    );
}

// --- hostile input --------------------------------------------------------

/// The stroker is run on awkward geometry for one property only: it must
/// always answer, never panic, and never emit a value that is not a number.
#[test]
fn assorted_hostile_strokes_never_panic() {
    let shapes = [
        SubPath::open(Vec::new()),
        SubPath::closed(Vec::new()),
        SubPath::open(alloc::vec![(0.0, 0.0)]),
        SubPath::closed(alloc::vec![(0.0, 0.0), (0.0, 0.0)]),
        SubPath::open(alloc::vec![(0.0, 0.0), (0.0, 0.0), (0.0, 0.0)]),
        SubPath::open(alloc::vec![(1e9, 1e9), (-1e9, -1e9)]),
        SubPath::closed(alloc::vec![(0.0, 0.0), (1.0, 0.0), (0.0, 0.0)]),
    ];
    let styles = [
        plain(1e6),
        plain(1e-9),
        StrokeStyle {
            width: 5.0,
            join: LineJoin::Miter,
            miter_limit: 0.0,
            ..StrokeStyle::default()
        },
        StrokeStyle {
            width: 5.0,
            cap: LineCap::Round,
            dashes: alloc::vec![1.0, 0.0],
            dash_offset: -1e9,
            ..StrokeStyle::default()
        },
    ];
    for shape in &shapes {
        for style in &styles {
            for tolerance in [0.0, -1.0, f64::NAN, 1e-12, 1e9] {
                if let Ok(pieces) =
                    stroke_outline(core::slice::from_ref(shape), style, tolerance, BUDGET)
                {
                    for point in pieces.iter().flat_map(|piece| piece.points.iter()) {
                        assert!(point.0.is_finite() && point.1.is_finite());
                    }
                }
            }
        }
    }
}
