//! The body frame: the one projection that serves every heading.
//!
//! The world is drawn top-down, but a figure standing in it is drawn as a
//! billboard seen from a shallower angle — otherwise a part offset along the
//! heading would read as a figure lying on the floor. [`FORESHORTEN`] is that
//! angle and the whole of the projection; the ground's own scale is the
//! camera's and is applied by the caller.
//!
//! # Why the turnaround is free
//!
//! A part carries a [`Body`] offset and is painted far-first by its projected
//! [`Projected::depth`]. Facing away, a face sorts behind the skull and is
//! covered; facing the camera it comes forward. Nothing asks which way the
//! figure points, so eight or sixteen headings need no artwork and no `cfg`.
//!
//! The depth key is deliberately not the screen row: a raised hand draws
//! higher without becoming further away, so sorting on the row would put it
//! behind the body it belongs to.
//!
//! # Why every outline is symmetric about its own vertical axis
//!
//! A rotation about the frame's `up` axis lies in the ground plane, and this
//! projection turns a ground-plane rotation into a shear rather than a screen
//! rotation. An outline symmetric about its vertical axis is unchanged by
//! that shear, which is what lets [`screen_turn`] ignore yaw entirely. An
//! asymmetric outline would need mirroring, and mirroring is the
//! per-direction branch the body frame exists to avoid.

use tairix_util::mathf;
use tairix_wintersun_net::value::Facing;

/// How much of a step *into* the scene shows as vertical screen movement.
///
/// The tangent of the figure's drawing elevation, about 35°: shallow enough
/// that a figure still reads in silhouette, steep enough that walking
/// up-screen reads as crossing a floor rather than rising into the air. The
/// authored value is this ratio rather than the angle, because the ratio is
/// what every line of the projection uses; the angle it corresponds to is
/// held to the doc by this module's tests.
pub const FORESHORTEN: f64 = 0.7;

/// A position or direction in the figure's own frame.
///
/// Authoring a body in these terms is what lets one skeleton serve every
/// heading. Lengths are figure-local pixels at [`STANDING_HEIGHT`].
///
/// [`STANDING_HEIGHT`]: crate::humanoid::STANDING_HEIGHT
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Body {
    /// Along the heading; positive is the way the figure faces.
    pub forward: f64,
    /// Across the heading; positive is to the figure's left.
    pub side: f64,
    /// Away from the ground.
    pub up: f64,
}

impl Body {
    /// The frame's origin.
    pub const ORIGIN: Self = Self::new(0.0, 0.0, 0.0);

    /// The frame's forward axis.
    pub const FORWARD: Self = Self::new(1.0, 0.0, 0.0);

    /// The frame's side axis, pointing to the figure's left.
    pub const SIDE: Self = Self::new(0.0, 1.0, 0.0);

    /// The frame's up axis.
    pub const UP: Self = Self::new(0.0, 0.0, 1.0);

    /// An offset in the figure's frame.
    #[must_use]
    pub const fn new(forward: f64, side: f64, up: f64) -> Self {
        Self { forward, side, up }
    }

    /// Whether every component is a finite number.
    #[must_use]
    pub fn is_real(self) -> bool {
        self.forward.is_finite() && self.side.is_finite() && self.up.is_finite()
    }

    /// Component-wise sum.
    #[must_use]
    pub fn plus(self, other: Self) -> Self {
        Self::new(
            self.forward + other.forward,
            self.side + other.side,
            self.up + other.up,
        )
    }

    /// This offset scaled uniformly.
    #[must_use]
    pub fn scaled(self, factor: f64) -> Self {
        Self::new(self.forward * factor, self.side * factor, self.up * factor)
    }

    /// Distance from the origin.
    #[must_use]
    pub fn length(self) -> f64 {
        mathf::hypot(mathf::hypot(self.forward, self.side), self.up)
    }

    /// Dot product.
    #[must_use]
    pub fn dot(self, other: Self) -> f64 {
        self.forward * other.forward + self.side * other.side + self.up * other.up
    }

    /// Cross product, right-handed over (`forward`, `side`, `up`).
    #[must_use]
    pub fn cross(self, other: Self) -> Self {
        Self::new(
            self.side * other.up - self.up * other.side,
            self.up * other.forward - self.forward * other.up,
            self.forward * other.side - self.side * other.forward,
        )
    }

    /// This direction turned `angle` radians about unit `axis`, right-handed.
    ///
    /// Rodrigues' rotation, so composing rotations needs no quaternion and no
    /// matrix inverse. `axis` must be a unit direction, which every axis of an
    /// orthonormal [`Basis`] is — the only caller.
    #[must_use]
    fn turned_about(self, axis: Self, angle: f64) -> Self {
        let (sin, cos) = (mathf::sin(angle), mathf::cos(angle));
        self.scaled(cos)
            .plus(axis.cross(self).scaled(sin))
            .plus(axis.scaled(axis.dot(self) * (1.0 - cos)))
    }
}

/// A rotation of one joint, about its own axes.
///
/// Applied intrinsically in the order yaw, pitch, roll — each about the axis
/// the previous rotation left — so a name keeps its anatomical meaning
/// whatever the others are doing: `pitch` always swings a limb fore-and-aft
/// in its own plane, never sideways.
///
/// Each is right-handed about the axis it names, with no sign flipped
/// anywhere in the transform. The consequence worth stating, because it
/// decides the sign of every limit a rig declares: a part *hangs below* its
/// joint, so it turns the opposite way to the joint's own `forward`. A
/// positive `pitch` tips the top forward and therefore swings a hanging limb
/// **backward**; a positive `roll` tips the top to the figure's right and
/// therefore swings a hanging limb to its **left**.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Rotation {
    /// About the `side` axis: a limb's fore-and-aft swing, a spine's bend.
    /// Positive leans the top forward and swings a hanging limb back.
    pub pitch: f64,
    /// About the `up` axis: a head's turn. Positive turns `forward` toward
    /// the figure's left.
    pub yaw: f64,
    /// About the `forward` axis: a limb splayed outward, a head tilted.
    /// Positive leans the top to the figure's right and swings a hanging limb
    /// to its left — so an outward splay is signed by which side it is on.
    pub roll: f64,
}

impl Rotation {
    /// No rotation.
    pub const REST: Self = Self {
        pitch: 0.0,
        yaw: 0.0,
        roll: 0.0,
    };

    /// A rotation about the three axes.
    #[must_use]
    pub const fn new(pitch: f64, yaw: f64, roll: f64) -> Self {
        Self { pitch, yaw, roll }
    }

    /// Whether every angle is a finite number.
    #[must_use]
    pub fn is_real(self) -> bool {
        self.pitch.is_finite() && self.yaw.is_finite() && self.roll.is_finite()
    }
}

/// An orthonormal frame, held as the images of the three body axes.
///
/// Three named directions rather than nine numbers in a row, so which
/// component means what is legible at every use and a composition cannot
/// transpose itself silently.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Basis {
    /// Where this frame's forward axis points in the frame it is held in.
    pub forward: Body,
    /// Where its side axis points.
    pub side: Body,
    /// Where its up axis points.
    pub up: Body,
}

impl Basis {
    /// The unrotated frame.
    pub const IDENTITY: Self = Self {
        forward: Body::FORWARD,
        side: Body::SIDE,
        up: Body::UP,
    };

    /// The frame `rotation` leaves.
    #[must_use]
    pub fn of(rotation: Rotation) -> Self {
        // Each axis is skipped when its angle is zero, which is the common
        // case: most joints drive one axis and rest on the other two.
        let mut basis = Self::IDENTITY;
        if rotation.yaw != 0.0 {
            basis = basis.turned_about(basis.up, rotation.yaw);
        }
        if rotation.pitch != 0.0 {
            basis = basis.turned_about(basis.side, rotation.pitch);
        }
        if rotation.roll != 0.0 {
            basis = basis.turned_about(basis.forward, rotation.roll);
        }
        basis
    }

    /// This frame turned `angle` radians about unit `axis`.
    #[must_use]
    fn turned_about(self, axis: Body, angle: f64) -> Self {
        Self {
            forward: self.forward.turned_about(axis, angle),
            side: self.side.turned_about(axis, angle),
            up: self.up.turned_about(axis, angle),
        }
    }

    /// `local`, which is stated in this frame, expressed in the frame this
    /// one is held in.
    #[must_use]
    pub fn apply(self, local: Body) -> Body {
        self.forward
            .scaled(local.forward)
            .plus(self.side.scaled(local.side))
            .plus(self.up.scaled(local.up))
    }

    /// `child`, which is stated in this frame, expressed in the frame this
    /// one is held in.
    #[must_use]
    pub fn compose(self, child: Self) -> Self {
        Self {
            forward: self.apply(child.forward),
            side: self.apply(child.side),
            up: self.apply(child.up),
        }
    }
}

/// Where a body-frame offset lands relative to the figure's ground point, and
/// how far into the scene it is.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Projected {
    /// Columns right of the figure's ground point.
    pub dx: f64,
    /// Rows below it.
    pub dy: f64,
    /// Distance toward the camera; larger is nearer.
    pub depth: f64,
}

/// Project body-frame `offset` for a figure facing `facing`.
///
/// The answer is in the same figure-local pixels `offset` is, relative to the
/// figure's ground point; scaling to the surface and adding the anchor is the
/// caller's one conversion.
#[must_use]
pub fn project(facing: Facing, offset: Body) -> Projected {
    let (across, into) = ground(facing, offset);
    Projected {
        // Into the scene moves down the screen, foreshortened; height moves
        // up it unforeshortened.
        dx: across,
        dy: into * FORESHORTEN - offset.up,
        depth: into,
    }
}

/// How far on screen a joint whose frame is `basis` has turned, for a figure
/// facing `facing`.
///
/// A flat outline cannot carry a rotation in three axes, so exactly the part
/// of it the screen can show is taken: the joint's rotation vector, projected
/// onto the two axes a screen rotation is visible about. A fore-and-aft swing
/// shows in full seen from the side and not at all seen down the depth axis,
/// where it happens in depth — so the billboard follows the projection with
/// no branch on which way the figure points. A turn about the vertical shows
/// not at all at any heading, which is what the symmetric-outline rule in
/// this module's own docs buys.
///
/// This is a projection of the rotation rather than a re-derivation of the
/// outline, deliberately: measuring instead the screen angle of the joint's
/// projected up axis is exact, but flips through half a turn where that axis
/// crosses the view direction, and a limb that pops is worse than one that
/// turns a few degrees short.
#[must_use]
pub fn screen_turn(facing: Facing, basis: Basis) -> f64 {
    // A rotation matrix's antisymmetric part is `sin a * n` and its trace
    // gives `cos a`, so one `atan2` recovers the angle and the rescale below
    // makes this `a * n` — the full turn a joint took, not a small-angle
    // estimate of it.
    let axis = Body::new(
        (basis.side.up - basis.up.side) * 0.5,
        (basis.up.forward - basis.forward.up) * 0.5,
        (basis.forward.side - basis.side.forward) * 0.5,
    );
    let sin = axis.length();
    let cos = (basis.forward.forward + basis.side.side + basis.up.up - 1.0) * 0.5;
    // The ratio tends to one as the rotation tends to rest, which is the
    // value to use where `sin a` is too small to divide by.
    let full = if sin > REST_EPSILON {
        mathf::atan2(sin, cos) / sin
    } else {
        1.0
    };
    let (east, south) = facing.unit_vector();
    // Clockwise-positive, matching a placed outline's own sense of turn: the
    // fore-and-aft component shows as the heading faces across the screen,
    // the splay component as it faces along the depth axis.
    (axis.side * east - axis.forward * south) * full
}

/// Below this, a frame is within rounding of rest — or of the half turn where
/// a rotation's axis is not recoverable from it at all, a figure folded onto
/// itself that no rig reaches from rest.
const REST_EPSILON: f64 = 1e-12;

/// `offset`'s displacement across the screen and into the scene, before the
/// depth axis is foreshortened.
fn ground(facing: Facing, offset: Body) -> (f64, f64) {
    // Zero faces east and the turn advances toward south, which is the sense
    // the world's own axes have. The figure's left is therefore a quarter
    // turn back from its heading: east when facing south.
    let (east, south) = facing.unit_vector();
    (
        offset.forward * east + offset.side * south,
        offset.forward * south - offset.side * east,
    )
}

#[cfg(test)]
#[path = "frame/tests.rs"]
mod tests;
