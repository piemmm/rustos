//! What a ring becomes once the joints have carried it.

use tairix_util::mathf;

use super::{
    carry, check, level, near, shaded, Hoop, Ring, Stretch, BANDS, LEVELS, MAX_RINGS, STRIP,
};
use crate::frame::{Basis, Body, Rotation};
use crate::species::SKIN;

const SLACK: f64 = 1e-9;

fn close(a: f64, b: f64) -> bool {
    mathf::fabs(a - b) <= SLACK
}

const STRAIGHT: [Ring; 3] = [
    Ring::new(Body::ORIGIN, 3.0, 2.0),
    Ring::new(Body::new(0.0, 0.0, -5.0), 3.0, 2.0),
    Ring::new(Body::new(0.0, 0.0, -10.0), 3.0, 2.0),
];

/// A limb spanning a bend: its middle ring half carried by the far joint,
/// its last ring wholly.
const SPAN: [Ring; 3] = [
    Ring::new(Body::ORIGIN, 3.0, 3.0),
    Ring::new(Body::new(0.0, 0.0, -5.0), 3.0, 3.0).bound(0.5),
    Ring::new(Body::new(0.0, 0.0, -10.0), 3.0, 3.0).bound(1.0),
];

fn rest() -> (Body, Basis) {
    (Body::ORIGIN, Basis::IDENTITY)
}

/// A ring is where its joint says it is, and its cross-section is square to
/// the surface it sits on.
#[test]
fn a_carried_ring_sits_where_its_joint_puts_it() {
    let hoops = carry(
        &STRAIGHT,
        Stretch::NONE,
        Body::new(1.0, 0.0, 0.0),
        rest(),
        None,
        None,
    )
    .expect("carries");
    assert_eq!(hoops.len(), STRAIGHT.len());
    for (hoop, ring) in hoops.iter().zip(STRAIGHT) {
        assert!(close(hoop.at.forward, 1.0) && close(hoop.at.up, ring.at.up));
        // The spine runs down, so the cross-section lies flat across it.
        assert!(close(hoop.wide.up, 0.0) && close(hoop.deep.up, 0.0));
        assert!(close(hoop.wide.length(), ring.wide));
        assert!(close(hoop.deep.length(), ring.deep));
        assert!(close(hoop.wide.dot(hoop.deep), 0.0), "the axes stay square");
    }
}

/// The whole point of the skin: a ring bound to the far joint follows it,
/// and one bound to neither end moves partway — which is what bends a limb
/// instead of creasing it.
#[test]
fn a_bound_ring_follows_the_joint_that_carries_it() {
    // The far joint hangs ten below and is turned a quarter of a radian.
    let rest_at = Body::new(0.0, 0.0, -10.0);
    let turned = Basis::of(Rotation::new(0.5, 0.0, 0.0));
    let far = (rest_at, turned);
    let hoops = carry(
        &SPAN,
        Stretch::NONE,
        Body::ORIGIN,
        rest(),
        Some(far),
        Some((rest_at, Basis::IDENTITY)),
    )
    .expect("carries");

    // The end ring is carried wholly by the far joint, so it lands exactly
    // where that joint's own frame puts it.
    assert!(close(hoops[2].at.forward, rest_at.forward) && close(hoops[2].at.up, rest_at.up));
    // The middle ring went halfway, which is the bend.
    let straight = carry(&SPAN, Stretch::NONE, Body::ORIGIN, rest(), None, None).expect("carries");
    let moved = hoops[1].at.plus(straight[1].at.scaled(-1.0)).length();
    assert!(moved > 0.0, "a bound ring must move at all");
    // And the ring bound to neither end did not.
    assert!(close(hoops[0].at.length(), 0.0));
}

/// The near side is the half a camera can see, so the point a quarter turn
/// into it is the nearest one on the ring.
#[test]
fn the_near_arc_is_the_half_facing_the_camera() {
    let hoop = Hoop {
        at: Body::ORIGIN,
        wide: Body::SIDE.scaled(4.0),
        deep: Body::FORWARD.scaled(3.0),
    };
    let view = Body::FORWARD;
    let start = near(hoop, view);
    let (nearest, _) = hoop.surface(start + STRIP * 2.0);
    assert!(
        nearest.forward > 3.0 - SLACK,
        "the middle of the near arc must be the nearest point"
    );
    for step in 0..=BANDS {
        // Bounded by BANDS.
        #[allow(clippy::cast_precision_loss, reason = "bounded by BANDS")]
        let along = step as f64;
        let (_, normal) = hoop.surface(start + STRIP * along);
        assert!(
            normal.dot(view) > -SLACK,
            "no point of the near arc may face away"
        );
    }
}

/// An ellipse's normal is its own, not a circle's: a flattened ring shades
/// as the flat surface it is.
#[test]
fn a_flattened_ring_normals_along_its_short_axis() {
    let hoop = Hoop {
        at: Body::ORIGIN,
        wide: Body::SIDE.scaled(10.0),
        deep: Body::FORWARD.scaled(1.0),
    };
    let (_, normal) = hoop.surface(core::f64::consts::FRAC_PI_4);
    assert!(
        normal.forward > 0.9,
        "a flat ring's rim faces out of its flat side, not along it"
    );
}

/// The ladder is what keeps the whole figure drawable in a small, exactly
/// known set of colours.
#[test]
fn the_shading_ladder_is_a_closed_set_that_darkens_away_from_the_light() {
    let light = Body::new(0.0, 0.0, -1.0);
    assert_eq!(level(Body::UP, light), LEVELS - 1, "straight at it is lit");
    assert_eq!(level(Body::new(0.0, 0.0, -1.0), light), 2, "away is shaded");

    let tone = SKIN[4];
    let mut last = 0u32;
    for step in 0..LEVELS {
        let lit = shaded(tone, step);
        let sum = u32::from(lit.r) + u32::from(lit.g) + u32::from(lit.b);
        assert!(step == 0 || sum > last, "the ladder must climb");
        assert_eq!(lit.a, tone.a, "shading is not opacity");
        last = sum;
    }
    assert_eq!(shaded(tone, LEVELS - 1), tone, "the top step is the tone");
    assert_eq!(
        shaded(tone, LEVELS + 9),
        shaded(tone, LEVELS - 1),
        "a step past the ladder lands on its top rather than past it"
    );
}

/// More rings than a part holds is refused rather than silently truncated
/// into a shape nobody authored.
#[test]
fn more_rings_than_a_part_holds_are_refused() {
    let many = [Ring::new(Body::ORIGIN, 1.0, 1.0); MAX_RINGS + 1];
    assert!(carry(&many, Stretch::NONE, Body::ORIGIN, rest(), None, None).is_err());
}

/// A ring that is not a real cross-section is refused where it is written.
#[test]
fn an_unreal_ring_is_not_real() {
    assert!(Ring::new(Body::ORIGIN, 1.0, 1.0).is_real());
    assert!(!Ring::new(Body::ORIGIN, f64::NAN, 1.0).is_real());
    assert!(!Ring::new(Body::ORIGIN, -1.0, 1.0).is_real());
    assert!(!Ring::new(Body::ORIGIN, 1.0, 1.0).bound(1.5).is_real());
    assert!(!Ring::new(Body::new(f64::INFINITY, 0.0, 0.0), 1.0, 1.0).is_real());
}

/// The one check every part and every piece of gear goes through: a run of
/// rings a part could not be drawn from is refused with the reason.
#[test]
fn a_run_of_rings_is_checked_before_anything_is_built_from_it() {
    assert_eq!(check(&STRAIGHT), Ok(()));
    assert_eq!(check(&[]), Err(crate::error::FigureError::GeometryUnreal));
    assert_eq!(
        check(&[Ring::new(Body::ORIGIN, f64::NAN, 1.0)]),
        Err(crate::error::FigureError::GeometryUnreal)
    );
    let many = [Ring::new(Body::ORIGIN, 1.0, 1.0); MAX_RINGS + 1];
    assert_eq!(check(&many), Err(crate::error::FigureError::TooManyParts));
}

/// A stretch scales a centre per axis and a cross-section per half-axis,
/// and leaves how far a ring is carried by the far joint alone.
#[test]
fn a_stretch_scales_each_axis_on_its_own() {
    let ring = Ring::new(Body::new(1.0, -2.0, 3.0), 4.0, 5.0).bound(0.25);
    let stretch = Stretch {
        forward: 2.0,
        side: 3.0,
        up: 0.5,
        wide: 1.5,
        deep: 0.25,
    };
    let stretched = stretch.apply(ring);
    assert_eq!(stretched.at, Body::new(2.0, -6.0, 1.5));
    assert!(close(stretched.wide, 6.0) && close(stretched.deep, 1.25));
    assert!(
        close(stretched.bind, 0.25),
        "a stretch is not a re-skinning"
    );
    assert_eq!(
        Stretch::NONE.apply(ring),
        ring,
        "no stretch leaves it as authored"
    );
    assert_eq!(stretch.point(ring.at), stretched.at);

    let doubled = stretch.scaled(2.0);
    assert!(close(doubled.forward, 4.0) && close(doubled.side, 6.0) && close(doubled.up, 1.0));
    assert!(close(doubled.wide, 3.0) && close(doubled.deep, 0.5));
}

/// Collapsing a part to a sheet or turning it inside out is not a build.
#[test]
fn a_stretch_that_is_not_finite_and_positive_is_not_real() {
    assert!(Stretch::NONE.is_real());
    assert!(Stretch::uniform(0.3).is_real());
    for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert!(
            !Stretch::uniform(bad).is_real(),
            "{bad} must not be a stretch"
        );
        let mut one = Stretch::NONE;
        one.deep = bad;
        assert!(
            !one.is_real(),
            "a single bad factor must refuse the stretch"
        );
    }
}

/// Carrying a stretched template is carrying the rings the stretch leaves:
/// the build reaches the surface through nothing but the stretch.
#[test]
fn carrying_through_a_stretch_carries_the_stretched_rings() {
    let stretch = Stretch {
        forward: 1.0,
        side: 1.0,
        up: 1.3,
        wide: 0.8,
        deep: 0.9,
    };
    let stretched = SPAN.map(|ring| stretch.apply(ring));
    let turned = (
        Body::new(0.0, 0.0, -13.0),
        Basis::of(Rotation::new(0.4, 0.0, 0.0)),
    );
    let rest_at = (Body::new(0.0, 0.0, -13.0), Basis::IDENTITY);
    let through = carry(
        &SPAN,
        stretch,
        Body::ORIGIN,
        rest(),
        Some(turned),
        Some(rest_at),
    )
    .expect("carries");
    let direct = carry(
        &stretched,
        Stretch::NONE,
        Body::ORIGIN,
        rest(),
        Some(turned),
        Some(rest_at),
    )
    .expect("carries");
    assert_eq!(through.len(), direct.len());
    for (one, other) in through.iter().zip(&direct) {
        assert_eq!(one, other);
    }
}

/// The skinning seam survives a build: a limb stretched along its spine,
/// hanging to a far joint moved by the same stretch, still ends exactly on
/// that joint however it is turned.
#[test]
fn a_stretched_limb_still_ends_on_its_stretched_joint() {
    let stretch = Stretch {
        forward: 1.0,
        side: 1.0,
        up: 1.25,
        wide: 1.1,
        deep: 1.1,
    };
    // `SPAN` ends ten below its joint, so the stretched far joint is there.
    let rest_at = Body::new(0.0, 0.0, -10.0 * 1.25);
    let far = (rest_at, Basis::of(Rotation::new(0.7, 0.0, 0.0)));
    let hoops = carry(
        &SPAN,
        stretch,
        Body::ORIGIN,
        rest(),
        Some(far),
        Some((rest_at, Basis::IDENTITY)),
    )
    .expect("carries");
    let end = hoops.last().expect("rings").at;
    assert!(close(end.forward, rest_at.forward) && close(end.up, rest_at.up));
}
