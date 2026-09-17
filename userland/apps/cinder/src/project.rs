//! The elevated camera every part of Cinder is drawn through.
//!
//! Not a flat side view: the camera looks *down* on the ground plane from a
//! shallow elevation, so a creature reads correctly whether it runs
//! left-to-right or up-and-down the screen. One constant expresses that
//! ([`GROUND_DEPTH`]) and everything — positions, part placement, the contact
//! shadow, the depth sort — is derived from it, so the projection has a single
//! definition.
//!
//! # Why the turnaround is free
//!
//! Parts carry a body-local position and are painted **far-first** by their
//! projected ground depth. Walking away, the face parts sort behind the head
//! and are simply covered; walking toward the camera they come forward. So
//! there is no front/back/side sprite set, no per-direction branch, and no
//! second code path to keep in step — the one sort does it.

use tairix_util::mathf;

/// The camera's elevation above the ground plane, in degrees.
///
/// Shallow enough that the creature still reads in profile — the silhouette
/// the mascot is recognised by — and steep enough that up-and-down movement
/// reads as travel across a floor rather than as rising into the air.
pub const ELEVATION_DEGREES: f64 = 35.0;

/// How much of a step *into* the screen shows as vertical screen movement.
///
/// The tangent of the camera's elevation, and the whole projection: a step
/// away from the camera covers this fraction of the screen distance the same
/// step sideways would. It is why Cinder crosses the screen faster
/// left-to-right than up-and-down, which is what makes a foreshortened floor
/// read as a floor.
pub const GROUND_DEPTH: f64 = 0.700_207_538_209_170_1;

/// A point on the ground plane, in screen pixels.
///
/// Ground positions are screen pixels rather than world units so there is no
/// second coordinate system to convert through: `x` is screen-horizontal and
/// `y` is the screen row the creature's feet rest on.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Ground {
    /// Screen-horizontal position of the feet.
    pub x: f64,
    /// Screen row the feet rest on.
    pub y: f64,
}

impl Ground {
    /// A ground point at `(x, y)`.
    #[must_use]
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// A position in the creature's own frame, before the camera sees it.
///
/// `forward` is along the creature's heading, `side` is to its left, and `up`
/// is away from the floor. Authoring a body in these terms is what lets one
/// skeleton serve every heading.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Body {
    /// Along the heading; positive is the way the creature faces.
    pub forward: f64,
    /// Across the heading; positive is to the creature's left.
    pub side: f64,
    /// Away from the floor.
    pub up: f64,
}

impl Body {
    /// A body-frame offset.
    #[must_use]
    pub const fn new(forward: f64, side: f64, up: f64) -> Self {
        Self { forward, side, up }
    }
}

/// Where a body-frame point lands on screen, and how far into the scene it
/// is.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Projected {
    /// Screen column.
    pub x: f64,
    /// Screen row.
    pub y: f64,
    /// Distance into the scene: larger is nearer the camera.
    ///
    /// The sort key, and *not* the screen row: a raised part draws higher on
    /// screen without becoming further away, so sorting on the row would put
    /// a lifted paw behind the body it belongs to.
    pub depth: f64,
}

/// Project body-frame `offset` on a creature standing at `at` with heading
/// `heading` radians.
///
/// Heading zero faces screen-right; it advances anticlockwise, so heading
/// `π/2` faces up the screen (away from the camera).
#[must_use]
pub fn project(at: Ground, heading: f64, offset: Body) -> Projected {
    let (sin, cos) = (mathf::sin(heading), mathf::cos(heading));
    // The ground displacement the offset represents, in screen-horizontal and
    // into-the-screen terms.
    let across = offset.forward * cos - offset.side * sin;
    let into = offset.forward * sin + offset.side * cos;
    Projected {
        x: at.x + across,
        // Into the screen moves *up* the screen, foreshortened by the camera;
        // height moves up the screen unforeshortened.
        y: at.y - into * GROUND_DEPTH - offset.up,
        depth: -into,
    }
}

/// How far a creature travelling `speed` along `heading` moves on screen in
/// one step.
///
/// The depth component is foreshortened exactly as a position is, so the same
/// speed covers less screen distance up-and-down than left-to-right — which
/// is what makes the floor read as a floor rather than as a wall.
#[must_use]
pub fn step(heading: f64, speed: f64) -> (f64, f64) {
    let (sin, cos) = (mathf::sin(heading), mathf::cos(heading));
    (speed * cos, -speed * sin * GROUND_DEPTH)
}

/// The heading that points from `from` to `to` across the ground plane.
///
/// The screen row difference is un-foreshortened first, so "walk towards the
/// pointer" aims at the floor position under the pointer rather than veering
/// as if the floor were vertical.
#[must_use]
pub fn heading_towards(from: Ground, to: Ground) -> f64 {
    let across = to.x - from.x;
    let into = (from.y - to.y) / GROUND_DEPTH;
    if across == 0.0 && into == 0.0 {
        return 0.0;
    }
    mathf::atan2(into, across)
}

/// The ground distance between two ground points.
///
/// Measured on the *floor*, so a creature judges "how far away is that?" the
/// way it would walk it rather than by how far apart the two look on screen.
#[must_use]
pub fn ground_distance(from: Ground, to: Ground) -> f64 {
    mathf::hypot(to.x - from.x, (to.y - from.y) / GROUND_DEPTH)
}

/// `angle` brought into `-π..=π`.
///
/// Subtraction rather than a remainder, because every caller here is already
/// within a turn or two of the range.
#[must_use]
pub fn wrap_angle(angle: f64) -> f64 {
    let mut wrapped = angle;
    while wrapped > core::f64::consts::PI {
        wrapped -= core::f64::consts::TAU;
    }
    while wrapped < -core::f64::consts::PI {
        wrapped += core::f64::consts::TAU;
    }
    wrapped
}

/// The smallest signed rotation from `from` to `to`, in radians.
///
/// Signed and shortest, so a creature turning to face something never spins
/// the long way round.
#[must_use]
pub fn turn_towards(from: f64, to: f64) -> f64 {
    wrap_angle(to - from)
}

/// The contact shadow's extent for a creature of `radius` standing `height`
/// above the floor.
///
/// Squashed by the camera exactly as the ground is, and fading with height:
/// the shadow is what tells the eye a jump left the floor, so it has to shrink
/// and lighten together rather than merely move.
#[must_use]
pub fn contact_shadow(radius: f64, height: f64) -> (f64, f64, u8) {
    // A shadow spreads a little and thins a lot as its caster rises. Both are
    // bounded so a high jump still leaves a mark rather than vanishing.
    let lift = mathf::clamp(height / SHADOW_FADE_HEIGHT, 0.0, 1.0);
    let spread = radius * (1.0 + lift * 0.35);
    let alpha = SHADOW_ALPHA_ON_FLOOR * (1.0 - lift * 0.75);
    let alpha = mathf::clamp(alpha, 0.0, 255.0);
    // The clamp above bounds the value into a byte exactly.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let alpha = alpha as u8;
    (spread, spread * GROUND_DEPTH, alpha)
}

/// The height at which a contact shadow has faded as far as it will.
const SHADOW_FADE_HEIGHT: f64 = 40.0;

/// How dark a contact shadow is with its caster on the floor.
const SHADOW_ALPHA_ON_FLOOR: f64 = 90.0;

#[cfg(test)]
#[path = "project_tests.rs"]
mod tests;
