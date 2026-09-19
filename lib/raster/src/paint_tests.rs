//! Unit tests for paint sampling.

use alloc::vec;

use super::{Gradient, GradientKind, GradientStop, Pattern, SpreadMethod, MAX_TILE_EXTENT};
use crate::affine::Affine;
use crate::color::Color;

const BLACK: Color = Color::rgb(0, 0, 0);
const WHITE: Color = Color::rgb(255, 255, 255);

/// Assert a sampled colour matches `want` to within one level per channel.
///
/// A radial parameter comes through a square root, so a ramp value that lands
/// exactly on a rounding boundary may round either way; a grey checked to the
/// level would be testing the last bit of the square root, not the ramp.
#[track_caller]
fn assert_shade_close(got: Color, want: Color) {
    let close = |a: u8, b: u8| a.abs_diff(b) <= 1;
    assert!(
        close(got.r, want.r) && close(got.g, want.g) && close(got.b, want.b) && got.a == want.a,
        "got {got:?}, want {want:?}"
    );
}

/// A black-to-white linear ramp over `0..=1` in the geometry's own
/// coordinates.
fn ramp(spread: SpreadMethod) -> Gradient {
    Gradient {
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
        spread,
        to_gradient: Affine::IDENTITY,
    }
}

#[test]
fn a_linear_ramp_interpolates_between_its_stops() {
    let gradient = ramp(SpreadMethod::Pad);
    assert_eq!(gradient.sample((0.0, 0.0)), BLACK);
    assert_eq!(gradient.sample((1.0, 0.0)), WHITE);
    assert_eq!(gradient.sample((0.5, 0.0)), Color::rgb(128, 128, 128));
    assert_eq!(gradient.sample((0.25, 0.0)), Color::rgb(64, 64, 64));
    // The parameter is the x coordinate alone, so y does not move the ramp.
    assert_eq!(gradient.sample((0.5, 900.0)), Color::rgb(128, 128, 128));
}

#[test]
fn a_ramp_interpolates_alpha_in_straight_form() {
    // A ramp fading to transparent keeps its hue: the midpoint is the same
    // colour at half alpha, not a darkened one.
    let gradient = Gradient {
        kind: GradientKind::Linear,
        stops: vec![
            GradientStop {
                offset: 0.0,
                color: Color::rgba(255, 0, 0, 255),
            },
            GradientStop {
                offset: 1.0,
                color: Color::rgba(255, 0, 0, 0),
            },
        ],
        spread: SpreadMethod::Pad,
        to_gradient: Affine::IDENTITY,
    };
    assert_eq!(gradient.sample((0.5, 0.0)), Color::rgba(255, 0, 0, 128));
}

#[test]
fn pad_holds_the_end_colours() {
    let gradient = ramp(SpreadMethod::Pad);
    assert_eq!(gradient.sample((-5.0, 0.0)), BLACK);
    assert_eq!(gradient.sample((6.25, 0.0)), WHITE);
}

#[test]
fn repeat_wraps_the_ramp() {
    let gradient = ramp(SpreadMethod::Repeat);
    // 1.25 and 2.25 are both a quarter of the way through a fresh ramp.
    assert_eq!(gradient.sample((1.25, 0.0)), Color::rgb(64, 64, 64));
    assert_eq!(gradient.sample((2.25, 0.0)), Color::rgb(64, 64, 64));
    // Negative parameters wrap the same way rather than clamping.
    assert_eq!(gradient.sample((-0.75, 0.0)), Color::rgb(64, 64, 64));
}

#[test]
fn reflect_mirrors_the_ramp() {
    let gradient = ramp(SpreadMethod::Reflect);
    // The second copy runs backwards, so a quarter past the end is a quarter
    // back from white.
    assert_eq!(gradient.sample((1.25, 0.0)), Color::rgb(191, 191, 191));
    assert_eq!(gradient.sample((2.25, 0.0)), Color::rgb(64, 64, 64));
    assert_eq!(gradient.sample((-0.25, 0.0)), Color::rgb(64, 64, 64));
}

#[test]
fn the_gradient_transform_places_the_ramp() {
    // A ramp running across x = 100..=200 in the shape's own coordinates is
    // expressed as the transform into canonical space, not as new stop maths.
    let gradient = Gradient {
        to_gradient: Affine::translate(-100.0, 0.0).then(Affine::scale(0.01, 1.0)),
        ..ramp(SpreadMethod::Pad)
    };
    assert_eq!(gradient.sample((100.0, 0.0)), BLACK);
    assert_eq!(gradient.sample((150.0, 0.0)), Color::rgb(128, 128, 128));
    assert_eq!(gradient.sample((200.0, 0.0)), WHITE);
}

#[test]
fn a_concentric_radial_ramps_with_distance_from_the_origin() {
    let gradient = Gradient {
        kind: GradientKind::Radial { focal: (0.0, 0.0) },
        ..ramp(SpreadMethod::Pad)
    };
    assert_eq!(gradient.sample((0.0, 0.0)), BLACK);
    assert_eq!(gradient.sample((1.0, 0.0)), WHITE);
    assert_eq!(gradient.sample((0.0, -1.0)), WHITE);
    assert_shade_close(gradient.sample((0.5, 0.0)), Color::rgb(128, 128, 128));
    assert_shade_close(gradient.sample((0.0, 0.5)), Color::rgb(128, 128, 128));
    // Outside the circle the pad spread holds the last stop.
    assert_eq!(gradient.sample((3.0, 4.0)), WHITE);
}

#[test]
fn a_focal_radial_starts_at_its_focus_and_ends_on_the_circle() {
    let gradient = Gradient {
        kind: GradientKind::Radial { focal: (0.5, 0.0) },
        ..ramp(SpreadMethod::Pad)
    };
    // The focus itself is the start of the ramp, wherever it sits.
    assert_eq!(gradient.sample((0.5, 0.0)), BLACK);
    // Every point on the unit circle is the end of the ramp, however far it
    // is from the focus.
    assert_eq!(gradient.sample((1.0, 0.0)), WHITE);
    assert_eq!(gradient.sample((-1.0, 0.0)), WHITE);
    assert_eq!(gradient.sample((0.0, 1.0)), WHITE);
    // Halfway from the focus to the circle along the short ray.
    assert_shade_close(gradient.sample((0.75, 0.0)), Color::rgb(128, 128, 128));
    // Halfway along the long ray is much further in distance, and still the
    // ramp's midpoint — which is the point of the focal formula.
    assert_shade_close(gradient.sample((-0.25, 0.0)), Color::rgb(128, 128, 128));
}

#[test]
fn a_focal_point_outside_the_circle_is_pulled_inside_it() {
    // SVG moves an outside focus just inside the edge; the sampler must stay
    // total rather than dividing by a zero-length ray.
    let gradient = Gradient {
        kind: GradientKind::Radial { focal: (40.0, 0.0) },
        ..ramp(SpreadMethod::Pad)
    };
    assert_eq!(gradient.sample((0.99, 0.0)), BLACK);
    assert_eq!(gradient.sample((-1.0, 0.0)), WHITE);
}

#[test]
fn a_ramp_with_no_stops_paints_nothing() {
    let gradient = Gradient {
        stops: vec![],
        ..ramp(SpreadMethod::Pad)
    };
    assert_eq!(gradient.sample((0.5, 0.0)), Color::TRANSPARENT);
}

#[test]
fn a_ramp_with_one_stop_paints_that_colour_everywhere() {
    let gradient = Gradient {
        stops: vec![GradientStop {
            offset: 0.5,
            color: WHITE,
        }],
        ..ramp(SpreadMethod::Repeat)
    };
    for x in [-9.0, 0.0, 0.5, 1.0, 500.0] {
        assert_eq!(gradient.sample((x, 0.0)), WHITE);
    }
}

#[test]
fn coincident_offsets_are_a_hard_stop() {
    let gradient = Gradient {
        stops: vec![
            GradientStop {
                offset: 0.0,
                color: BLACK,
            },
            GradientStop {
                offset: 0.5,
                color: BLACK,
            },
            GradientStop {
                offset: 0.5,
                color: WHITE,
            },
            GradientStop {
                offset: 1.0,
                color: WHITE,
            },
        ],
        ..ramp(SpreadMethod::Pad)
    };
    assert_eq!(gradient.sample((0.25, 0.0)), BLACK);
    assert_eq!(gradient.sample((0.75, 0.0)), WHITE);
    assert_eq!(gradient.sample((0.5, 0.0)), WHITE);
}

#[test]
fn a_degenerate_transform_or_wild_point_still_answers_a_colour() {
    // A collapsed gradient transform and coordinates at the extremes must
    // resolve to an end colour, never a `NaN` the compositor cannot store.
    let collapsed = Gradient {
        to_gradient: Affine::scale(0.0, 0.0),
        ..ramp(SpreadMethod::Reflect)
    };
    assert_eq!(collapsed.sample((f64::MAX, f64::MIN)), BLACK);
    let radial = Gradient {
        kind: GradientKind::Radial { focal: (0.9, 0.0) },
        ..ramp(SpreadMethod::Repeat)
    };
    let sampled = radial.sample((f64::MAX, f64::MAX));
    assert!(sampled == BLACK || sampled == WHITE, "{sampled:?}");
    assert_eq!(radial.sample((f64::NAN, 0.0)), BLACK);
}

/// A pattern whose unit tile is the design-grid square `to_tile` maps back to.
fn tiling(to_tile: Affine) -> Pattern {
    Pattern {
        content: vec![],
        to_tile,
    }
}

#[test]
fn a_tile_is_rendered_at_the_density_it_is_read_at() {
    // One tile spans 64 contour units and a pixel spans 2, so the tile wants
    // 32 pixels a side.
    let pattern = tiling(Affine::scale(1.0 / 64.0, 1.0 / 64.0));
    assert_eq!(pattern.tile_extent((2.0, 2.0)), Some((32, 32)));
    // Half the density on one axis alone halves that axis alone.
    assert_eq!(pattern.tile_extent((2.0, 4.0)), Some((32, 16)));
}

#[test]
fn a_rotated_tile_is_sized_along_its_own_axes() {
    // A tile 80 contour units square, turned a quarter turn: its edges are
    // still 80 units long whichever way they point.
    let placement = Affine::scale(80.0, 80.0).then(Affine::rotate_degrees(90.0));
    let pattern = tiling(placement.invert().expect("a rotation inverts"));
    assert_eq!(pattern.tile_extent((1.0, 1.0)), Some((80, 80)));
}

#[test]
fn an_absurd_tile_is_clamped_rather_than_allocated_for() {
    let pattern = tiling(Affine::scale(1e-5, 1e-5));
    assert_eq!(
        pattern.tile_extent((1.0, 1.0)),
        Some((MAX_TILE_EXTENT, MAX_TILE_EXTENT))
    );
    // A tile smaller than a pixel still has a pixel to be drawn in.
    let tiny = tiling(Affine::scale(1e6, 1e6));
    assert_eq!(tiny.tile_extent((1.0, 1.0)), Some((1, 1)));
}

#[test]
fn a_collapsed_or_unreadable_tiling_has_no_tile() {
    assert_eq!(
        tiling(Affine::scale(0.0, 1.0)).tile_extent((1.0, 1.0)),
        None
    );
    // A tile so large the map back from it reads as collapsed area has no
    // repeat to render either.
    assert_eq!(
        tiling(Affine::scale(1e-9, 1e-9)).tile_extent((1.0, 1.0)),
        None
    );
    let pattern = tiling(Affine::IDENTITY);
    assert_eq!(pattern.tile_extent((0.0, 1.0)), None);
    assert_eq!(pattern.tile_extent((f64::NAN, 1.0)), None);
    assert_eq!(pattern.tile_extent((1.0, f64::INFINITY)), None);
}

#[test]
fn a_tile_position_repeats_in_both_directions() {
    let pattern = tiling(Affine::scale(0.25, 0.5));
    assert_eq!(pattern.tile_position((0.0, 0.0)), Some((0.0, 0.0)));
    assert_eq!(pattern.tile_position((2.0, 1.0)), Some((0.5, 0.5)));
    // Four units on is the next tile at the same place, and so is four back.
    assert_eq!(pattern.tile_position((6.0, 1.0)), Some((0.5, 0.5)));
    assert_eq!(pattern.tile_position((-2.0, -1.0)), Some((0.5, 0.5)));
}

#[test]
fn a_tile_position_refuses_a_point_the_map_sends_nowhere() {
    let pattern = tiling(Affine::scale(f64::MAX, f64::MAX));
    assert_eq!(pattern.tile_position((f64::MAX, 0.0)), None);
    assert_eq!(
        tiling(Affine::IDENTITY).tile_position((f64::NAN, 0.0)),
        None
    );
}
