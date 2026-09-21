//! Where a contact shadow lands, and what shape it is when it gets there.

use core::f64::consts::{FRAC_PI_2, FRAC_PI_4};

use tairix_raster::shape::Shape;
use tairix_raster::Color;
use tairix_util::mathf;

use super::{Contact, Light};
use crate::error::FigureError;
use crate::frame::FORESHORTEN;

const SLACK: f64 = 1e-9;
const TONE: Color = Color::rgba(0, 0, 0, 160);

fn close(a: f64, b: f64) -> bool {
    mathf::fabs(a - b) <= SLACK
}

fn contact() -> Contact {
    Contact::new(9.0, TONE).expect("a real contact")
}

fn overhead() -> Light {
    Light::new(1.0, 0.0, FRAC_PI_2).expect("a real light")
}

/// The extent of a placed superellipse along the screen axes.
#[track_caller]
fn extent(shape: Shape, turn: f64) -> (f64, f64) {
    let Shape::Superellipse { rx, ry, .. } = shape else {
        panic!("a contact shadow is an ellipse");
    };
    let (sin, cos) = (mathf::sin(turn), mathf::cos(turn));
    // The bounding half-extents of an ellipse turned by `turn`.
    (
        mathf::hypot(rx * cos, ry * sin),
        mathf::hypot(rx * sin, ry * cos),
    )
}

#[test]
fn an_unreal_light_is_refused() {
    for (across, into) in [(0.0, 0.0), (f64::NAN, 1.0), (1.0, f64::INFINITY)] {
        assert_eq!(
            Light::new(across, into, 1.0).map(|_| ()),
            Err(FigureError::LightUnreal),
            "a direction of ({across}, {into}) must be refused"
        );
    }
    for elevation in [0.0, -0.5, FRAC_PI_2 + 0.01, f64::NAN] {
        assert_eq!(
            Light::new(1.0, 0.0, elevation).map(|_| ()),
            Err(FigureError::LightUnreal),
            "an elevation of {elevation} must be refused"
        );
    }
}

#[test]
fn an_unreal_contact_lift_or_scale_is_refused() {
    for radius in [0.0, -1.0, f64::NAN] {
        assert_eq!(
            Contact::new(radius, TONE).map(|_| ()),
            Err(FigureError::GeometryUnreal),
            "a radius of {radius} must be refused"
        );
    }
    let contact = contact();
    assert_eq!(
        contact
            .cast(overhead(), f64::NAN, 1.0, (0.0, 0.0))
            .map(|_| ()),
        Err(FigureError::GeometryUnreal)
    );
    assert_eq!(
        contact
            .cast(overhead(), 0.0, 1.0, (f64::NAN, 0.0))
            .map(|_| ()),
        Err(FigureError::GeometryUnreal)
    );
    for scale in [0.0, -1.0, f64::NAN] {
        assert_eq!(
            contact.cast(overhead(), 0.0, scale, (0.0, 0.0)).map(|_| ()),
            Err(FigureError::ScaleUnreal),
            "a scale of {scale} must be refused"
        );
    }
}

/// A light straight overhead throws no shadow sideways, so what lands is the
/// footprint itself, foreshortened by the ground it lies on.
#[test]
fn an_overhead_light_lays_the_footprint_flat_on_the_ground() {
    let placed = contact()
        .cast(overhead(), 0.0, 1.0, (100.0, 200.0))
        .expect("it casts");
    assert!(close(placed.x, 100.0) && close(placed.y, 200.0));
    let (wide, tall) = extent(placed.shape, placed.turn);
    assert!(close(wide, 9.0), "across the screen {wide}");
    assert!(close(tall, 9.0 * FORESHORTEN), "into the scene {tall}");
    assert_eq!(placed.color.a, TONE.a, "a foot on the ground casts in full");
}

/// The composition the module exists for: a raked ellipse foreshortened is
/// not the raked ellipse with a squashed axis, and the solved axes must
/// match the ground ellipse actually projected.
#[test]
fn the_screen_ellipse_matches_the_ground_ellipse_projected() {
    let contact = Contact::new(7.0, TONE).expect("a real contact");
    for bearing in [0.0, 0.3, FRAC_PI_4, 1.2, 2.5, -1.9] {
        let elevation = 0.6;
        let light =
            Light::new(mathf::cos(bearing), mathf::sin(bearing), elevation).expect("a real light");
        let placed = contact.cast(light, 0.0, 1.0, (0.0, 0.0)).expect("it casts");
        let (wide, tall) = extent(placed.shape, placed.turn);

        // The same ellipse worked out the long way: sweep the ground circle,
        // rake it, project it, and take the extremes.
        let rake = 1.0 / mathf::sin(elevation);
        let (mut widest, mut tallest) = (0.0f64, 0.0f64);
        let mut step = 0;
        while step < 720 {
            let angle = f64::from(step) * core::f64::consts::TAU / 720.0;
            let (along, across) = (7.0 * rake * mathf::cos(angle), 7.0 * mathf::sin(angle));
            let ground = (
                along * mathf::cos(bearing) - across * mathf::sin(bearing),
                along * mathf::sin(bearing) + across * mathf::cos(bearing),
            );
            widest = mathf::fmax(widest, mathf::fabs(ground.0));
            tallest = mathf::fmax(tallest, mathf::fabs(ground.1 * FORESHORTEN));
            step += 1;
        }
        assert!(
            mathf::fabs(wide - widest) < 1e-3,
            "bearing {bearing}: solved {wide} swept {widest}"
        );
        assert!(
            mathf::fabs(tall - tallest) < 1e-3,
            "bearing {bearing}: solved {tall} swept {tallest}"
        );
    }
}

/// A lower sun rakes the shadow out along its own bearing, and never the
/// other way.
#[test]
fn a_lower_light_rakes_the_shadow_out_along_its_bearing() {
    let contact = contact();
    let high = contact
        .cast(
            Light::new(1.0, 0.0, 1.3).expect("a real light"),
            0.0,
            1.0,
            (0.0, 0.0),
        )
        .expect("it casts");
    let low = contact
        .cast(
            Light::new(1.0, 0.0, 0.35).expect("a real light"),
            0.0,
            1.0,
            (0.0, 0.0),
        )
        .expect("it casts");
    assert!(extent(low.shape, low.turn).0 > extent(high.shape, high.turn).0);
    assert!(close(
        extent(low.shape, low.turn).1,
        extent(high.shape, high.turn).1
    ));
}

/// A sun on the horizon makes a cast shadow, which is a different primitive;
/// this one stops raking rather than growing without bound.
#[test]
fn a_light_near_the_horizon_stops_raking_rather_than_running_away() {
    let contact = contact();
    let mut last = 0.0;
    for elevation in [0.5, 0.3, 0.2, 0.1, 0.01, 1e-6] {
        let light = Light::new(1.0, 0.0, elevation).expect("a real light");
        let placed = contact.cast(light, 0.0, 1.0, (0.0, 0.0)).expect("it casts");
        let (wide, _) = extent(placed.shape, placed.turn);
        assert!(
            wide.is_finite() && wide <= 9.0 * 4.0 + SLACK,
            "width {wide}"
        );
        assert!(wide >= last, "raking must not reverse");
        last = wide;
    }
}

/// The readability trick: a figure in the air leaves its shadow behind on
/// the ground, sliding away from the light and thinning as it goes.
#[test]
fn a_rising_figure_leaves_its_shadow_behind_and_fades_it() {
    let contact = contact();
    let light = Light::new(0.6, 0.8, 0.9).expect("a real light");
    let grounded = contact.cast(light, 0.0, 1.0, (0.0, 0.0)).expect("it casts");

    let (mut away, mut alpha) = (0.0, f64::from(grounded.color.a) + 1.0);
    for lift in [0.0, 2.0, 6.0, 15.0, 40.0] {
        let placed = contact
            .cast(light, lift, 1.0, (0.0, 0.0))
            .expect("it casts");
        let distance = mathf::hypot(placed.x, placed.y);
        assert!(distance >= away, "a rising figure's shadow must slide away");
        assert!(
            f64::from(placed.color.a) < alpha || lift == 0.0,
            "and thin as it goes: {} at lift {lift}",
            placed.color.a
        );
        // It slides along the light, never against it.
        if lift > 0.0 {
            assert!(placed.x > 0.0 && placed.y > 0.0, "thrown the wrong way");
        }
        away = distance;
        alpha = f64::from(placed.color.a);
    }
    // A figure far enough up casts almost nothing, and never a negative
    // amount of anything.
    let far = contact
        .cast(light, 400.0, 1.0, (0.0, 0.0))
        .expect("it casts");
    assert!(far.color.a < 10);
}

/// A shadow below the ground is still a shadow on it: a figure sunk into the
/// terrain does not cast a growing one back out.
#[test]
fn a_figure_below_its_ground_point_casts_as_if_on_it() {
    let contact = contact();
    let light = Light::new(1.0, 0.0, 0.8).expect("a real light");
    let on = contact.cast(light, 0.0, 1.0, (0.0, 0.0)).expect("it casts");
    let under = contact
        .cast(light, -5.0, 1.0, (0.0, 0.0))
        .expect("it casts");
    assert_eq!(on, under);
}

#[test]
fn scale_takes_the_whole_shadow_with_it() {
    let contact = contact();
    let light = Light::new(0.3, -0.9, 0.7).expect("a real light");
    let unit = contact.cast(light, 6.0, 1.0, (0.0, 0.0)).expect("it casts");
    let half = contact.cast(light, 6.0, 0.5, (0.0, 0.0)).expect("it casts");
    assert!(close(half.x, unit.x * 0.5) && close(half.y, unit.y * 0.5));
    let (unit_wide, unit_tall) = extent(unit.shape, unit.turn);
    let (half_wide, half_tall) = extent(half.shape, half.turn);
    assert!(close(half_wide, unit_wide * 0.5) && close(half_tall, unit_tall * 0.5));
    assert_eq!(half.color, unit.color, "scale is size, not opacity");
}
