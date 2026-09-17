//! Shape tests: that each form is the form it claims to be, that every
//! outline fits the buffer it is traced into, and that a fill lands where the
//! part was placed.

use super::{fill, Placed, Scratch, Shape, MAX_VERTICES};
use tairix_inline::ArrayVec;
use tairix_raster::{Color, Pixel, Surface};
use tairix_util::mathf;

const RED: Color = Color::rgb(0xFF, 0x00, 0x00);

fn traced(shape: Shape) -> ArrayVec<(f64, f64), MAX_VERTICES> {
    let mut out = ArrayVec::new();
    shape.trace(&mut out);
    out
}

fn blank(side: u32) -> Surface {
    Surface::filled(side, side, Pixel::TRANSPARENT).expect("a test surface allocates")
}

fn covered(surface: &Surface) -> usize {
    surface.pixels().iter().filter(|pixel| pixel.a > 0).count()
}

/// Every shape a body part can be, at the sizes the skeleton uses.
fn every_shape() -> [Shape; 5] {
    [
        Shape::Fur { radius: 9.0 },
        Shape::Mass {
            rx: 13.5,
            ry: 13.0,
            square: 0.3,
        },
        Shape::Limb {
            length: 31.0,
            top: 5.4,
            foot: 3.6,
        },
        Shape::Ear {
            half_width: 6.4,
            height: 13.0,
            lean: 3.4,
        },
        Shape::Drape {
            rx: 12.5,
            ry: 16.0,
            folds: 3,
        },
    ]
}

#[test]
fn every_outline_fits_the_buffer_it_is_traced_into() {
    // The build-time assertions cover the authored parameters; this covers the
    // extremes, including a fold count past the bound.
    let extremes = [
        Shape::Mass {
            rx: 0.0,
            ry: 0.0,
            square: 1.0,
        },
        Shape::Limb {
            length: -5.0,
            top: 0.0,
            foot: 0.0,
        },
        Shape::Ear {
            half_width: 0.0,
            height: 0.0,
            lean: 0.0,
        },
        Shape::Drape {
            rx: 1.0,
            ry: 1.0,
            folds: 0,
        },
        Shape::Drape {
            rx: 1.0,
            ry: 1.0,
            folds: u32::MAX,
        },
    ];
    for shape in every_shape().into_iter().chain(extremes) {
        let outline = traced(shape);
        assert!(
            outline.len() <= MAX_VERTICES,
            "{shape:?} traced {} vertices into a buffer of {MAX_VERTICES}",
            outline.len()
        );
    }
}

#[test]
fn a_hem_asking_for_more_lobes_than_the_bound_is_gathered_coarsely_not_truncated() {
    // Truncating the ring would close the panel through the wrong vertices and
    // draw a shape nobody authored, so the fold count is clamped instead.
    let wild = traced(Shape::Drape {
        rx: 10.0,
        ry: 10.0,
        folds: u32::MAX,
    });
    let bounded = traced(Shape::Drape {
        rx: 10.0,
        ry: 10.0,
        folds: 4,
    });
    assert_eq!(wild.len(), bounded.len());
    assert!(wild.len() >= 3, "a panel must still be a fillable ring");
}

#[test]
fn fur_has_no_outline_because_it_is_a_splat() {
    assert!(traced(Shape::Fur { radius: 9.0 }).is_empty());
}

#[test]
fn a_mass_at_zero_squareness_is_an_ellipse_and_at_full_reaches_its_corners() {
    let radii = (10.0, 6.0);
    let round = traced(Shape::Mass {
        rx: radii.0,
        ry: radii.1,
        square: 0.0,
    });
    let boxy = traced(Shape::Mass {
        rx: radii.0,
        ry: radii.1,
        square: 1.0,
    });
    // An ellipse never leaves its own radii; a squared mass reaches the corner.
    let furthest = |outline: &ArrayVec<(f64, f64), MAX_VERTICES>| {
        outline
            .iter()
            .map(|&(x, y)| mathf::hypot(x, y))
            .fold(0.0_f64, f64::max)
    };
    assert!(furthest(&round) <= radii.0 + 1.0e-9);
    assert!(
        furthest(&boxy) > furthest(&round),
        "a squared mass must push past the ellipse it started as"
    );
    // The flats stay put: the ray straight out to the right lands on `rx`
    // whatever the squareness, or the whole rim would bulge.
    assert!((round[0].0 - radii.0).abs() < 1.0e-9);
    assert!((boxy[0].0 - radii.0).abs() < 1.0e-9);
}

#[test]
fn every_outline_is_symmetric_about_its_own_vertical_axis() {
    // This is what keeps the turnaround free: an asymmetric outline would need
    // mirroring, and mirroring is the per-direction branch the camera exists to
    // avoid. An ear is the deliberate exception — it leans, and the pair leans
    // apart rather than either one being handed.
    for shape in every_shape() {
        if matches!(shape, Shape::Ear { .. } | Shape::Fur { .. }) {
            continue;
        }
        let outline = traced(shape);
        let width = |pick: fn(&(f64, f64)) -> bool| {
            outline
                .iter()
                .filter(|vertex| pick(vertex))
                .map(|&(x, _)| mathf::fabs(x))
                .fold(0.0_f64, f64::max)
        };
        let left = width(|&(x, _)| x < 0.0);
        let right = width(|&(x, _)| x > 0.0);
        assert!(
            (left - right).abs() < 1.0e-9,
            "{shape:?} reaches {left} left and {right} right"
        );
    }
}

#[test]
fn a_limb_hangs_below_its_joint_so_a_swing_pivots_where_it_meets_the_body() {
    let outline = traced(Shape::Limb {
        length: 30.0,
        top: 5.0,
        foot: 3.0,
    });
    let highest = outline
        .iter()
        .map(|&(_, y)| y)
        .fold(f64::NEG_INFINITY, f64::max);
    let lowest = outline
        .iter()
        .map(|&(_, y)| y)
        .fold(f64::INFINITY, f64::min);
    assert!(
        (highest - 0.0).abs() < 1.0e-9,
        "the joint is the local origin, so nothing rises above it"
    );
    assert!(
        lowest <= -30.0,
        "the foot must reach the limb's full length ({lowest})"
    );
}

#[test]
fn an_ear_tucks_its_base_below_the_skull_so_no_seam_shows() {
    let outline = traced(Shape::Ear {
        half_width: 6.0,
        height: 12.0,
        lean: 3.0,
    });
    let lowest = outline
        .iter()
        .map(|&(_, y)| y)
        .fold(f64::INFINITY, f64::min);
    assert!(lowest < 0.0, "the base sits under its anchor ({lowest})");
    let tip = outline
        .iter()
        .map(|&(_, y)| y)
        .fold(f64::NEG_INFINITY, f64::max);
    assert!((tip - 12.0).abs() < 1.0e-9, "the tip reaches its height");
}

#[test]
fn a_hem_is_not_a_straight_edge() {
    // A panel with a flat hem reads as a card taped to the creature.
    let outline = traced(Shape::Drape {
        rx: 10.0,
        ry: 12.0,
        folds: 3,
    });
    let hem: alloc::vec::Vec<f64> = outline
        .iter()
        .filter(|&&(_, y)| y < -1.0)
        .map(|&(_, y)| y)
        .collect();
    assert!(hem.len() > 3, "the hem is traced, not a single edge");
    let highest = hem.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let lowest = hem.iter().copied().fold(f64::INFINITY, f64::min);
    assert!(
        highest - lowest > 1.0,
        "the hem must rise and fall to read as cloth"
    );
}

#[test]
fn a_fill_lands_where_the_part_was_placed() {
    let mut surface = blank(64);
    let mut scratch = Scratch::new();
    fill(
        &mut surface,
        &Placed {
            x: 20.0,
            y: 20.0,
            turn: 0.0,
            shape: Shape::Mass {
                rx: 6.0,
                ry: 6.0,
                square: 0.0,
            },
            color: RED,
            seed: 0,
        },
        &mut scratch,
    );
    assert!(covered(&surface) > 0, "the shape must actually be drawn");
    assert!(
        surface.get(20, 20).is_some_and(|pixel| pixel.a > 0),
        "the shape's centre is its placed point"
    );
    assert_eq!(
        surface.get(50, 50).map(|pixel| pixel.a),
        Some(0),
        "and nothing is drawn where the shape is not"
    );
}

#[test]
fn a_turn_rotates_the_outline_about_its_own_origin() {
    // The origin must hold still, because a limb's origin is its joint.
    let draw = |turn: f64| {
        let mut surface = blank(96);
        let mut scratch = Scratch::new();
        fill(
            &mut surface,
            &Placed {
                x: 48.0,
                y: 48.0,
                turn,
                shape: Shape::Limb {
                    length: 30.0,
                    top: 5.0,
                    foot: 3.0,
                },
                color: RED,
                seed: 0,
            },
            &mut scratch,
        );
        surface
    };
    let square = draw(0.0);
    let leaned = draw(0.5);
    assert!(covered(&square) > 0 && covered(&leaned) > 0);
    for surface in [&square, &leaned] {
        assert!(
            surface.get(48, 50).is_some_and(|pixel| pixel.a > 0),
            "the joint stays covered however the limb swings"
        );
    }
    assert_ne!(
        square.pixels(),
        leaned.pixels(),
        "a turn must actually move the limb"
    );
}

#[test]
fn a_transparent_colour_and_a_degenerate_shape_draw_nothing() {
    let mut surface = blank(32);
    let mut scratch = Scratch::new();
    for placed in [
        Placed {
            x: 16.0,
            y: 16.0,
            turn: 0.0,
            shape: Shape::Mass {
                rx: 8.0,
                ry: 8.0,
                square: 0.0,
            },
            color: Color::TRANSPARENT,
            seed: 0,
        },
        Placed {
            x: 16.0,
            y: 16.0,
            turn: 0.0,
            shape: Shape::Fur { radius: 8.0 },
            color: RED,
            seed: 0,
        },
    ] {
        fill(&mut surface, &placed, &mut scratch);
    }
    assert_eq!(covered(&surface), 0);
}

#[test]
fn a_shape_placed_far_off_the_surface_stays_off_it() {
    // A wrapped sub-pixel coordinate would fold the shape back across the
    // canvas, which is a stripe of fur where nothing should be.
    let mut surface = blank(32);
    let mut scratch = Scratch::new();
    for at in [-1.0e12, 1.0e12, f64::MAX, f64::MIN] {
        fill(
            &mut surface,
            &Placed {
                x: at,
                y: at,
                turn: 0.0,
                shape: Shape::Mass {
                    rx: 8.0,
                    ry: 8.0,
                    square: 0.0,
                },
                color: RED,
                seed: 0,
            },
            &mut scratch,
        );
    }
    assert_eq!(covered(&surface), 0);
}

#[test]
fn reach_bounds_every_shapes_own_outline() {
    for shape in every_shape() {
        let reach = shape.reach();
        for &(x, y) in &traced(shape) {
            assert!(
                mathf::hypot(x, y) <= reach + 1.0e-9,
                "{shape:?} reaches {} but claims {reach}",
                mathf::hypot(x, y)
            );
        }
    }
}
