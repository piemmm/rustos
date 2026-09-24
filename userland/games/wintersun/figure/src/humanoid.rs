//! The humanoid rig: the one skeleton every species stands on.
//!
//! Proportioned in percentages of [`STANDING_HEIGHT`], so every offset below
//! reads directly as a fraction of the figure's own height and a reviewer can
//! check a shoulder sits at 82% without converting anything.
//!
//! # Built from a record
//!
//! A figure's [`Identity`] reaches this skeleton through two things only:
//! the offsets between its joints, which are values, and a [`Stretch`] on
//! each surface's authored rings. A longer limb is a longer bone and a
//! stretched surface rather than a different mesh, so the rings stay
//! borrowed first-party templates, and species and features choose between
//! templates rather than bending one.
//!
//! # Height is height
//!
//! Limb and head proportion change a figure's shape and not its stature: the
//! whole skeleton is scaled so the crown stands exactly where the height
//! setting puts it, with the sole on the ground. A long-legged figure of a
//! given height has the shorter trunk, which is what a proportion is.
//!
//! # One table, two sides
//!
//! A left and a right limb are the same numbers with one sign flipped, so
//! they are built from one pass over [`Side::BOTH`] rather than authored
//! twice — two tables would be two things to keep in step, and a rig whose
//! sides disagreed would read as a limp nobody animated.
//!
//! # The joint that bears a limb carries its mass
//!
//! There is no separate shoulder and upper-arm joint, and no separate hip and
//! thigh joint: the shoulder *is* where the arm swings and where the trunk
//! carries its deltoid, from one entry. That is what makes a gap at the
//! shoulder unspellable rather than merely unlikely, and it is why
//! [`Rig::new`] refuses a bearing joint that draws nothing.
//!
//! [`Rig::new`]: crate::rig::Rig::new

use tairix_inline::ArrayVec;
use tairix_util::mathf;

use crate::error::FigureError;
use crate::frame::{Basis, Body, Rotation};
use crate::identity::{Features, Identity, TailForm};
use crate::joint::{Joint, JointId, Limit, Limits, MAX_JOINTS};
use crate::mesh::{self, Ring, Stretch};
use crate::plant::Leg;
use crate::pose::{Mask, Param};
use crate::rig::{Part, Rig, MAX_PARTS};
use crate::rigging::{Axis, Drive, Rigging};
use crate::socket::{Mount, Side, Socket};
use crate::tint::Tint;

mod feature;

/// How tall the reference figure stands, in the figure-local pixels the rig
/// is authored in.
///
/// A hundred, so an offset is a percentage of height. A record's height
/// setting is a factor on this, and a caller draws a figure at any size by
/// scaling: the factor is the height it wants over this.
pub const STANDING_HEIGHT: f64 = 100.0;

/// How many joints the humanoid has.
pub const JOINT_COUNT: usize = 18;

/// How many surfaces every figure has before its features: trunk, head,
/// arms and legs.
pub const BODY_PARTS: usize = 21;

/// How many surfaces the richest figure has: the body, two eyes, two ears
/// each with a marked face, two horns, a two-part style of hair, and a tail
/// with a marked tip.
pub const MOST_PARTS: usize = BODY_PARTS + 12;

const _: () = assert!(JOINT_COUNT <= MAX_JOINTS);
const _: () = assert!(MOST_PARTS == MAX_PARTS);

/// How far from its ground point the largest figure any record describes
/// reaches at rest, rounded up to a whole unit.
///
/// What a view drawing every figure at one scale sizes that scale by, so the
/// largest fits and every other reads at its own size against it. A test
/// holds every admissible figure inside it, and the largest within a unit of
/// it.
pub const MOST_REACH: f64 = 120.0;

/// The humanoid's named joints.
///
/// Named so a caller poses a knee without counting the table, and so a clip
/// authored against one humanoid rig means the same thing on another.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Bone {
    /// The root: the whole figure hangs from it.
    Pelvis,
    /// The lower spine.
    Waist,
    /// The upper spine, carrying the shoulders and the neck.
    Chest,
    /// The base of the neck.
    Neck,
    /// The atlas, where the skull pivots.
    Head,
    /// Where an arm swings and the trunk carries its deltoid.
    Shoulder(Side),
    /// The elbow.
    Elbow(Side),
    /// The wrist.
    Wrist(Side),
    /// Where a leg swings and the trunk carries its haunch.
    Hip(Side),
    /// The knee.
    Knee(Side),
    /// The ankle.
    Ankle(Side),
    /// The root of a tail.
    ///
    /// Every figure has one, tailed or not, so every species shares one
    /// skeleton and every clip plays on all of them; on a figure without a
    /// tail it simply carries nothing.
    Tail,
}

impl Bone {
    /// Every bone, in the order the rig's joint table holds them.
    pub const ALL: [Self; JOINT_COUNT] = [
        Self::Pelvis,
        Self::Waist,
        Self::Chest,
        Self::Neck,
        Self::Head,
        Self::Shoulder(Side::Left),
        Self::Elbow(Side::Left),
        Self::Wrist(Side::Left),
        Self::Shoulder(Side::Right),
        Self::Elbow(Side::Right),
        Self::Wrist(Side::Right),
        Self::Hip(Side::Left),
        Self::Knee(Side::Left),
        Self::Ankle(Side::Left),
        Self::Hip(Side::Right),
        Self::Knee(Side::Right),
        Self::Ankle(Side::Right),
        Self::Tail,
    ];

    /// The joint it names.
    ///
    /// A side's three bones sit together, so a chain's parent always precedes
    /// its child whichever side it is on.
    #[must_use]
    pub const fn joint(self) -> JointId {
        JointId::new(match self {
            Self::Pelvis => 0,
            Self::Waist => 1,
            Self::Chest => 2,
            Self::Neck => 3,
            Self::Head => 4,
            Self::Shoulder(side) => 5 + 3 * side as u8,
            Self::Elbow(side) => 6 + 3 * side as u8,
            Self::Wrist(side) => 7 + 3 * side as u8,
            Self::Hip(side) => 11 + 3 * side as u8,
            Self::Knee(side) => 12 + 3 * side as u8,
            Self::Ankle(side) => 13 + 3 * side as u8,
            Self::Tail => 17,
        })
    }

    /// Its position in the rig's joint table.
    #[must_use]
    pub const fn index(self) -> usize {
        self.joint().index()
    }
}

/// Assemble the humanoid for `identity`.
///
/// A caller builds it once and holds it: a rig does not change between
/// frames, and a palette edit re-tints it through [`Rig::retint`] rather
/// than rebuilding it.
///
/// # Errors
///
/// Only what [`Rig::new`] refuses, and only if the tables here are edited
/// into something inconsistent — every identity can be built, which the
/// crate's tests hold across every species' extremes and every form.
///
/// [`Rig::new`]: crate::rig::Rig::new
/// [`Rig::retint`]: crate::rig::Rig::retint
pub fn rig(identity: &Identity) -> Result<Rig, FigureError> {
    let body = Physique::of(identity)?;
    let features = identity.spec().features;
    let mut joints: ArrayVec<Joint, MAX_JOINTS> = ArrayVec::new();
    let mut parts: ArrayVec<Part, MAX_PARTS> = ArrayVec::new();
    let mut mounts: ArrayVec<(Socket, Mount), { Socket::COUNT }> = ArrayVec::new();

    spine(&mut joints, &body)?;
    arms(&mut joints, &body)?;
    leg_joints(&mut joints, &body)?;
    tail_joint(&mut joints, &body)?;
    trunk(&mut parts, &body)?;
    head(&mut parts, &body, features)?;
    limbs(&mut parts, &body)?;
    tail(&mut parts, &body, features.tail)?;
    sockets(&mut mounts, &body)?;

    Rig::new(&joints, &parts, &mounts, identity.tints())
}

/// What a record's proportions come to on this skeleton.
///
/// Every factor but `scale` is a proportion on the reference figure; `scale`
/// then turns reference units into the figure's own, so every length below
/// is written in reference units and passes through it once.
#[derive(Copy, Clone, Debug)]
struct Physique {
    /// Figure-local units per reference unit: what puts the crown at the
    /// stated height.
    scale: f64,
    /// Where the pelvis sits, in reference units.
    pelvis: f64,
    /// Breadth of the chest and shoulders.
    chest: f64,
    /// Breadth of the pelvis and hips.
    hips: f64,
    /// Depth of the trunk, front to back.
    depth: f64,
    /// Girth of a limb, which takes part of the body's.
    limb: f64,
    /// Girth of the neck, which takes less of it again.
    neck: f64,
    /// Limb length.
    limbs: f64,
    /// Head size.
    head: f64,
    /// Hair fullness.
    volume: f64,
}

impl Physique {
    fn of(identity: &Identity) -> Result<Self, FigureError> {
        let wanted = identity.proportions();
        let pelvis = sole_depth()? + (THIGH_LENGTH + SHANK_LENGTH) * wanted.limbs + HIP_DROP;
        let crown = pelvis
            + WAIST_RISE
            + CHEST_RISE
            + NECK_RISE
            + ATLAS_RISE
            + feature::CROWN * wanted.head;
        Ok(Self {
            scale: wanted.height * STANDING_HEIGHT / crown,
            pelvis,
            chest: wanted.girth * (1.0 + wanted.taper),
            hips: wanted.girth * (1.0 - wanted.taper),
            depth: wanted.girth,
            limb: 1.0 + (wanted.girth - 1.0) * LIMB_GIRTH,
            neck: 1.0 + (wanted.girth - 1.0) * NECK_GIRTH,
            limbs: wanted.limbs,
            head: wanted.head,
            volume: wanted.volume,
        })
    }

    /// A position given in reference units, in the figure's own.
    fn at(&self, forward: f64, side: f64, up: f64) -> Body {
        Body::new(forward, side, up).scaled(self.scale)
    }

    /// A trunk or limb surface stretched `along` its spine, `wide` across it
    /// and `deep` through it.
    fn stretch(&self, along: f64, wide: f64, deep: f64) -> Stretch {
        Stretch {
            forward: 1.0,
            side: 1.0,
            up: along,
            wide,
            deep,
        }
        .scaled(self.scale)
    }

    /// Every length of a surface scaled by `factor`.
    fn uniform(&self, factor: f64) -> Stretch {
        Stretch::uniform(factor * self.scale)
    }

    /// Anything the head carries, which grows with the head.
    fn on_head(&self, at: Body) -> Body {
        at.scaled(self.head * self.scale)
    }
}

/// How far below its ankle the reference foot reaches, which is how high an
/// ankle stands for its sole to meet the ground.
///
/// Read off the foot's own rings rather than stated beside them, so a
/// reshaped foot cannot leave the figure standing in or above the floor.
fn sole_depth() -> Result<f64, FigureError> {
    let hoops = mesh::carry(
        &FOOT,
        Stretch::NONE,
        Body::new(0.0, 0.0, -FOOT_DROP),
        (Body::ORIGIN, Basis::IDENTITY),
        None,
        None,
    )?;
    let mut lowest = 0.0;
    for hoop in &hoops {
        // A cross-section is an ellipse in space, so how far down it reaches
        // is the vertical reach of its two half-axes together.
        let reach = mathf::hypot(hoop.wide.up, hoop.deep.up);
        lowest = mathf::fmin(lowest, hoop.at.up - reach);
    }
    Ok(-lowest)
}

/// The spine, bottom to top. Each joint's offset is the length of the
/// segment below it, so moving one moves everything it carries.
fn spine(joints: &mut ArrayVec<Joint, MAX_JOINTS>, body: &Physique) -> Result<(), FigureError> {
    push_joint(
        joints,
        Joint::new(None, body.at(0.0, 0.0, body.pelvis), spine_limits()?),
    )?;
    push_joint(
        joints,
        Joint::new(
            Some(Bone::Pelvis.joint()),
            body.at(0.0, 0.0, WAIST_RISE),
            spine_limits()?,
        ),
    )?;
    push_joint(
        joints,
        Joint::new(
            Some(Bone::Waist.joint()),
            body.at(0.0, 0.0, CHEST_RISE),
            spine_limits()?,
        ),
    )?;
    push_joint(
        joints,
        Joint::new(
            Some(Bone::Chest.joint()),
            body.at(0.0, 0.0, NECK_RISE),
            neck_limits()?,
        ),
    )?;
    push_joint(
        joints,
        Joint::new(
            Some(Bone::Neck.joint()),
            body.at(0.0, 0.0, ATLAS_RISE),
            skull_limits()?,
        ),
    )?;

    Ok(())
}

/// Both arms, mirrored from one pass.
fn arms(joints: &mut ArrayVec<Joint, MAX_JOINTS>, body: &Physique) -> Result<(), FigureError> {
    for side in Side::BOTH {
        let across = side.across();
        push_joint(
            joints,
            Joint::new(
                Some(Bone::Chest.joint()),
                body.at(0.0, across * SHOULDER_WIDTH * body.chest, SHOULDER_RISE),
                shoulder_limits(side)?,
            ),
        )?;
        push_joint(
            joints,
            Joint::new(
                Some(Bone::Shoulder(side).joint()),
                body.at(0.0, 0.0, -UPPER_ARM_LENGTH * body.limbs),
                elbow_limits()?,
            ),
        )?;
        push_joint(
            joints,
            Joint::new(
                Some(Bone::Elbow(side).joint()),
                body.at(0.0, 0.0, -FOREARM_LENGTH * body.limbs),
                wrist_limits()?,
            ),
        )?;
    }

    Ok(())
}

/// Both legs, mirrored from one pass.
fn leg_joints(
    joints: &mut ArrayVec<Joint, MAX_JOINTS>,
    body: &Physique,
) -> Result<(), FigureError> {
    for side in Side::BOTH {
        let across = side.across();
        push_joint(
            joints,
            Joint::new(
                Some(Bone::Pelvis.joint()),
                body.at(0.0, across * HIP_WIDTH * body.hips, -HIP_DROP),
                hip_limits(side)?,
            ),
        )?;
        push_joint(
            joints,
            Joint::new(
                Some(Bone::Hip(side).joint()),
                body.at(0.0, 0.0, -THIGH_LENGTH * body.limbs),
                knee_limits()?,
            ),
        )?;
        push_joint(
            joints,
            Joint::new(
                Some(Bone::Knee(side).joint()),
                body.at(0.0, 0.0, -SHANK_LENGTH * body.limbs),
                ankle_limits()?,
            ),
        )?;
    }

    Ok(())
}

/// The tail's root, low on the back of the pelvis, resting pitched so a tail
/// hangs down its frame the way a limb hangs down its joint — which is what
/// lets a sway layer drive it exactly as it drives a hanging hem.
fn tail_joint(
    joints: &mut ArrayVec<Joint, MAX_JOINTS>,
    body: &Physique,
) -> Result<(), FigureError> {
    push_joint(
        joints,
        Joint::new(
            Some(Bone::Pelvis.joint()),
            body.at(-TAIL_BACK * body.depth, 0.0, -TAIL_DROP),
            tail_limits()?,
        )
        .oriented(Rotation::new(TAIL_HANG, 0.0, 0.0)),
    )
}

/// A ring at `up` above the part's origin, `wide` across and `deep`
/// through.
const fn hoop(up: f64, wide: f64, deep: f64) -> Ring {
    Ring::new(Body::new(0.0, 0.0, up), wide, deep)
}

/// The trunk and neck, bottom to top. Each part's upper rings are carried by
/// the joint above it, so the torso bends through the waist and the chest
/// rather than telescoping at them.
fn trunk(parts: &mut ArrayVec<Part, MAX_PARTS>, body: &Physique) -> Result<(), FigureError> {
    const PELVIS: [Ring; 5] = [
        hoop(-7.5, 4.0, 3.0),
        hoop(-5.0, 9.4, 6.6),
        hoop(0.0, 10.4, 7.2),
        hoop(5.0, 10.0, 6.9).bound(0.35),
        hoop(9.0, 9.6, 6.6).bound(1.00),
    ];
    const WAIST: [Ring; 3] = [
        hoop(0.0, 9.6, 6.6),
        hoop(6.0, 10.8, 7.1).bound(0.30),
        hoop(13.0, 12.0, 7.6).bound(1.00),
    ];
    const CHEST: [Ring; 4] = [
        hoop(0.0, 12.0, 7.6),
        hoop(5.0, 12.4, 7.8),
        hoop(9.0, 10.2, 6.8).bound(0.35),
        hoop(11.5, 5.4, 4.4).bound(1.00),
    ];
    const NECK: [Ring; 2] = [hoop(0.0, 4.6, 4.4), hoop(4.0, 4.3, 4.1).bound(1.00)];

    push_part(
        parts,
        Part::new(Bone::Pelvis.joint(), Body::ORIGIN, &PELVIS, Tint::Trousers)?
            .stretched(body.stretch(1.0, body.hips, body.depth))?
            .spanning(Bone::Waist.joint()),
    )?;
    push_part(
        parts,
        Part::new(Bone::Waist.joint(), Body::ORIGIN, &WAIST, Tint::Accent)?
            .stretched(body.stretch(1.0, f64::midpoint(body.hips, body.chest), body.depth))?
            .spanning(Bone::Chest.joint()),
    )?;
    push_part(
        parts,
        Part::new(Bone::Chest.joint(), Body::ORIGIN, &CHEST, Tint::Accent)?
            .stretched(body.stretch(1.0, body.chest, body.depth))?
            .spanning(Bone::Neck.joint()),
    )?;
    push_part(
        parts,
        Part::new(Bone::Neck.joint(), Body::ORIGIN, &NECK, Tint::Skin)?
            .stretched(body.stretch(1.0, body.neck, body.neck))?
            .spanning(Bone::Head.joint()),
    )
}

/// The skull and everything it carries, in the order a depth tie paints them.
///
/// The skull and the cap of hair over it sort by one point, so they always
/// tie and the cap, coming after, covers the skull from every side. A mass of
/// hair down the back sorts behind that point and comes before the skull, so
/// it covers the head only where the back of the head is the nearer.
fn head(
    parts: &mut ArrayVec<Part, MAX_PARTS>,
    body: &Physique,
    features: Features,
) -> Result<(), FigureError> {
    let skull = body.uniform(body.head);
    let skull_rings = feature::skull_for(features.face);
    let centre = feature::centre(skull_rings);
    let behind_centre = centre.plus(Body::new(-feature::BEHIND, 0.0, 0.0));
    let hair = features.hair.map(feature::hair_for);
    let fullness = Stretch {
        forward: body.volume,
        side: body.volume,
        up: 1.0,
        wide: body.volume,
        deep: body.volume,
    }
    .scaled(body.head * body.scale);

    if let Some(behind) = hair.and_then(|style| style.behind) {
        push_part(
            parts,
            Part::new(Bone::Head.joint(), Body::ORIGIN, behind, Tint::Hair)?
                .stretched(fullness)?
                .sorted_at(behind_centre)?,
        )?;
    }
    push_part(
        parts,
        Part::new(Bone::Head.joint(), Body::ORIGIN, skull_rings, Tint::Skin)?
            .stretched(skull)?
            .sorted_at(centre)?,
    )?;
    if let Some(style) = hair {
        push_part(
            parts,
            Part::new(Bone::Head.joint(), Body::ORIGIN, style.over, Tint::Hair)?
                .stretched(fullness)?
                .sorted_at(centre)?,
        )?;
    }

    for side in Side::BOTH {
        let at = Body::new(
            feature::EYE_AT.forward,
            feature::EYE_AT.side * side.across(),
            feature::EYE_AT.up,
        );
        push_part(
            parts,
            Part::new(
                Bone::Head.joint(),
                body.on_head(at),
                feature::eye_for(features.eyes),
                Tint::Eyes,
            )?
            .stretched(skull)?,
        )?;
    }
    for side in Side::BOTH {
        let ear = feature::ear_for(features.ears, side);
        let at = body.on_head(ear.at);
        push_part(
            parts,
            Part::new(Bone::Head.joint(), at, ear.outer, ear.tint)?.stretched(skull)?,
        )?;
        if let Some(inner) = ear.inner {
            push_part(
                parts,
                Part::new(Bone::Head.joint(), at, inner, Tint::Markings)?.stretched(skull)?,
            )?;
        }
    }
    if let Some(form) = features.horns {
        for side in Side::BOTH {
            let (at, rings) = feature::horn_for(form, side);
            push_part(
                parts,
                Part::new(Bone::Head.joint(), body.on_head(at), rings, Tint::Markings)?
                    .stretched(skull)?,
            )?;
        }
    }

    Ok(())
}

/// The arms and the legs, each mirrored from one pass.
fn limbs(parts: &mut ArrayVec<Part, MAX_PARTS>, body: &Physique) -> Result<(), FigureError> {
    arm_parts(parts, body)?;
    leg_parts(parts, body)
}

/// Both arms, and the sleeve each swings out of.
fn arm_parts(parts: &mut ArrayVec<Part, MAX_PARTS>, body: &Physique) -> Result<(), FigureError> {
    // The sleeve belongs to the shoulder joint, not to the arm, so the arm
    // can swing without opening a gap where it meets the trunk.
    // The hem ends flush with the arm beneath it: a cap narrower than what
    // it covers shows its own rim through the limb, which reads as a frill
    // nobody authored.
    const SLEEVE: [Ring; 5] = [
        hoop(5.2, 1.8, 1.8),
        hoop(3.5, 4.2, 4.2),
        hoop(0.0, 4.9, 4.9),
        hoop(-4.0, 4.6, 4.6),
        hoop(-6.0, 3.85, 3.85),
    ];
    const UPPER_ARM: [Ring; 4] = [
        hoop(-2.0, 3.6, 3.6),
        hoop(-5.0, 3.9, 3.9),
        hoop(-12.0, 3.4, 3.4).bound(0.30),
        hoop(-UPPER_ARM_LENGTH, 3.0, 3.0).bound(1.00),
    ];
    const FOREARM: [Ring; 3] = [
        hoop(0.0, 3.0, 3.0),
        hoop(-8.0, 2.7, 2.7).bound(0.30),
        hoop(-FOREARM_LENGTH, 2.4, 2.4).bound(1.00),
    ];
    const HAND: [Ring; 4] = [
        hoop(0.0, 1.8, 1.4),
        hoop(-2.5, 2.7, 2.0),
        hoop(-5.5, 2.6, 1.9),
        hoop(-7.8, 1.0, 0.8),
    ];
    let limb = body.stretch(body.limbs, body.limb, body.limb);
    for side in Side::BOTH {
        push_part(
            parts,
            Part::new(
                Bone::Shoulder(side).joint(),
                Body::ORIGIN,
                &SLEEVE,
                Tint::Accent,
            )?
            .stretched(body.stretch(1.0, body.limb, body.limb))?,
        )?;
        push_part(
            parts,
            Part::new(
                Bone::Shoulder(side).joint(),
                Body::ORIGIN,
                &UPPER_ARM,
                Tint::Skin,
            )?
            .stretched(limb)?
            .spanning(Bone::Elbow(side).joint()),
        )?;
        push_part(
            parts,
            Part::new(
                Bone::Elbow(side).joint(),
                Body::ORIGIN,
                &FOREARM,
                Tint::Skin,
            )?
            .stretched(limb)?
            .spanning(Bone::Wrist(side).joint()),
        )?;
        push_part(
            parts,
            Part::new(Bone::Wrist(side).joint(), Body::ORIGIN, &HAND, Tint::Skin)?
                .stretched(body.uniform(1.0))?,
        )?;
    }

    Ok(())
}

/// A foot runs forward from its ankle rather than hanging below it, and is
/// about a seventh of a person's height: drawn much shorter it stops reading
/// as a foot at all at icon size.
const FOOT: [Ring; 5] = [
    Ring::new(Body::new(-3.4, 0.0, 0.3), 1.2, 1.2),
    Ring::new(Body::new(-2.0, 0.0, 0.0), 2.6, 2.3),
    Ring::new(Body::new(2.5, 0.0, -0.4), 2.8, 2.4),
    Ring::new(Body::new(6.8, 0.0, -0.8), 2.3, 1.9),
    Ring::new(Body::new(9.4, 0.0, -1.0), 0.9, 0.8),
];

/// Both legs and their boots, mirrored from one pass.
fn leg_parts(parts: &mut ArrayVec<Part, MAX_PARTS>, body: &Physique) -> Result<(), FigureError> {
    const HAUNCH: [Ring; 4] = [
        hoop(4.5, 2.6, 2.6),
        hoop(2.5, 5.8, 5.8),
        hoop(-2.0, 5.8, 5.8),
        hoop(-5.0, 4.4, 4.4),
    ];
    const THIGH: [Ring; 4] = [
        hoop(-1.0, 4.8, 4.8),
        hoop(-5.0, 5.2, 5.2),
        hoop(-13.0, 4.3, 4.3).bound(0.30),
        hoop(-THIGH_LENGTH, 3.6, 3.6).bound(1.00),
    ];
    const SHANK: [Ring; 4] = [
        hoop(0.0, 3.6, 3.6),
        hoop(-8.0, 3.3, 3.3),
        hoop(-17.0, 2.7, 2.7).bound(0.35),
        hoop(-SHANK_LENGTH, 2.3, 2.3).bound(1.00),
    ];

    let limb = body.stretch(body.limbs, body.limb, body.limb);
    for side in Side::BOTH {
        let across = side.across();
        push_part(
            parts,
            Part::new(
                Bone::Hip(side).joint(),
                Body::ORIGIN,
                &HAUNCH,
                Tint::Trousers,
            )?
            .stretched(body.stretch(1.0, body.limb, body.limb))?,
        )?;
        push_part(
            parts,
            Part::new(
                Bone::Hip(side).joint(),
                Body::ORIGIN,
                &THIGH,
                Tint::Trousers,
            )?
            .stretched(limb)?
            .spanning(Bone::Knee(side).joint()),
        )?;
        push_part(
            parts,
            Part::new(
                Bone::Knee(side).joint(),
                Body::ORIGIN,
                &SHANK,
                Tint::Trousers,
            )?
            .stretched(limb)?
            .spanning(Bone::Ankle(side).joint()),
        )?;
        push_part(
            parts,
            Part::new(
                Bone::Ankle(side).joint(),
                body.at(0.0, across * FOOT_SPLAY, -FOOT_DROP),
                &FOOT,
                Tint::Leather,
            )?
            .stretched(body.uniform(1.0))?,
        )?;
    }

    Ok(())
}

/// The tail, if the figure has one, and its marked tip.
fn tail(
    parts: &mut ArrayVec<Part, MAX_PARTS>,
    body: &Physique,
    form: Option<TailForm>,
) -> Result<(), FigureError> {
    let Some(form) = form else { return Ok(()) };
    let shape = feature::tail_for(form);
    let girth = body.stretch(1.0, body.limb, body.limb);
    push_part(
        parts,
        Part::new(Bone::Tail.joint(), Body::ORIGIN, shape.root, Tint::Skin)?.stretched(girth)?,
    )?;
    if let Some(tip) = shape.tip {
        push_part(
            parts,
            Part::new(Bone::Tail.joint(), Body::ORIGIN, tip, Tint::Markings)?.stretched(girth)?,
        )?;
    }
    Ok(())
}

/// Where equipment hangs, and how large the body is at each place.
fn sockets(
    mounts: &mut ArrayVec<(Socket, Mount), { Socket::COUNT }>,
    body: &Physique,
) -> Result<(), FigureError> {
    mount(
        mounts,
        Socket::Head,
        Bone::Head,
        body.on_head(Body::new(0.0, 0.0, SKULL_RISE)),
        body.head * body.scale,
    )?;
    mount(
        mounts,
        Socket::Back,
        Bone::Chest,
        body.at(-6.0 * body.depth, 0.0, 6.0),
        body.depth * body.scale,
    )?;
    mount(
        mounts,
        Socket::MainHand,
        Bone::Wrist(Side::Right),
        body.at(2.0, 0.0, -3.4),
        body.scale,
    )?;
    mount(
        mounts,
        Socket::OffHand,
        Bone::Wrist(Side::Left),
        body.at(2.0, 0.0, -3.4),
        body.scale,
    )?;
    for side in Side::BOTH {
        let across = side.across();
        mount(
            mounts,
            Socket::Shoulder(side),
            Bone::Shoulder(side),
            body.at(0.0, across * 1.6, 2.0),
            body.limb * body.scale,
        )?;
        mount(
            mounts,
            Socket::Hip(side),
            Bone::Hip(side),
            body.at(0.0, across * 2.0, 2.0),
            body.limb * body.scale,
        )?;
        mount(
            mounts,
            Socket::Foot(side),
            Bone::Ankle(side),
            body.at(1.6, 0.0, -1.2),
            body.scale,
        )?;
    }

    Ok(())
}

/// Pelvis to waist.
const WAIST_RISE: f64 = 9.0;

/// Waist to chest.
const CHEST_RISE: f64 = 13.0;

/// Chest to the base of the neck.
const NECK_RISE: f64 = 10.0;

/// The base of the neck to the atlas.
const ATLAS_RISE: f64 = 4.0;

/// How far the skull's centre sits above the atlas.
const SKULL_RISE: f64 = 6.0;

/// How far out from the spine a shoulder sits.
const SHOULDER_WIDTH: f64 = 11.5;

/// How far above the chest joint a shoulder sits.
const SHOULDER_RISE: f64 = 8.0;

/// How far out from the spine a hip sits.
const HIP_WIDTH: f64 = 9.0;

/// How far below the pelvis joint a hip sits.
const HIP_DROP: f64 = 1.0;

/// How far behind the pelvis a tail roots, and how far below it.
const TAIL_BACK: f64 = 6.0;
const TAIL_DROP: f64 = 1.5;

/// How far a tail's rest leans back from hanging straight down: about 55°,
/// so it hangs back and low rather than straight down the legs.
const TAIL_HANG: f64 = 0.95;

/// How far below the ankle a foot's frame sits, and how far out.
const FOOT_DROP: f64 = 2.2;
const FOOT_SPLAY: f64 = 0.3;

/// Shoulder to elbow.
const UPPER_ARM_LENGTH: f64 = 19.0;

/// Elbow to wrist.
const FOREARM_LENGTH: f64 = 15.0;

/// Hip to knee.
pub(crate) const THIGH_LENGTH: f64 = 23.0;

/// Knee to ankle.
pub(crate) const SHANK_LENGTH: f64 = 24.0;

/// How much of the body's girth a limb takes: a heavy build thickens its
/// arms and legs, but by less than its trunk.
const LIMB_GIRTH: f64 = 0.75;

/// How much of it the neck takes.
const NECK_GIRTH: f64 = 0.6;

/// A spine segment: a little of everything, and not much of any of it.
fn spine_limits() -> Result<Limits, FigureError> {
    Ok(Limits::new(
        Limit::symmetric(0.30)?,
        Limit::symmetric(0.35)?,
        Limit::symmetric(0.20)?,
    ))
}

/// The neck turns freely and tips less.
fn neck_limits() -> Result<Limits, FigureError> {
    Ok(Limits::new(
        Limit::symmetric(0.40)?,
        Limit::symmetric(0.70)?,
        Limit::symmetric(0.25)?,
    ))
}

/// The skull adds the rest of a look, and the tilt a listening head has.
fn skull_limits() -> Result<Limits, FigureError> {
    Ok(Limits::new(
        Limit::symmetric(0.45)?,
        Limit::symmetric(0.60)?,
        Limit::symmetric(0.30)?,
    ))
}

/// A shoulder: nearly a ball joint, and the widest range in the rig.
///
/// Handed, because a splay is outward on both sides and outward is a
/// different sign on each: a positive roll swings a hanging limb to the
/// figure's left.
fn shoulder_limits(side: Side) -> Result<Limits, FigureError> {
    Ok(Limits::new(
        // An arm raises far forward and much less behind, and forward is a
        // negative pitch because the limb hangs below the joint.
        Limit::new(-2.10, 1.20)?,
        Limit::symmetric(0.80)?,
        splay(side, 2.60, 0.35)?,
    ))
}

/// An elbow only closes, and only one way. Letting it open past straight is
/// the defect the limits exist to catch.
fn elbow_limits() -> Result<Limits, FigureError> {
    Ok(Limits::new(
        Limit::new(-2.40, 0.0)?,
        Limit::FIXED,
        Limit::symmetric(0.25)?,
    ))
}

/// A wrist bends and twists a little.
fn wrist_limits() -> Result<Limits, FigureError> {
    Ok(Limits::new(
        Limit::symmetric(1.10)?,
        Limit::symmetric(0.30)?,
        Limit::symmetric(0.60)?,
    ))
}

/// A hip swings well forward, much less back, and splays outward only.
fn hip_limits(side: Side) -> Result<Limits, FigureError> {
    Ok(Limits::new(
        Limit::new(-2.00, 0.60)?,
        Limit::symmetric(0.45)?,
        splay(side, 0.80, 0.20)?,
    ))
}

/// A knee only folds backward, which is the other joint a bad clip inverts.
fn knee_limits() -> Result<Limits, FigureError> {
    Ok(Limits::new(
        Limit::new(0.0, 2.40)?,
        Limit::FIXED,
        Limit::FIXED,
    ))
}

/// A roll limit of `outward` away from the body and `inward` across it, for a
/// limb on `side`.
fn splay(side: Side, outward: f64, inward: f64) -> Result<Limit, FigureError> {
    match side {
        Side::Left => Limit::new(-inward, outward),
        Side::Right => Limit::new(-outward, inward),
    }
}

/// An ankle flexes and rolls a little, which is what lets a foot meet a
/// slope.
fn ankle_limits() -> Result<Limits, FigureError> {
    Ok(Limits::new(
        Limit::symmetric(0.70)?,
        Limit::symmetric(0.20)?,
        Limit::symmetric(0.35)?,
    ))
}

/// A tail lifts well above where it hangs, droops a little below it, and
/// swings to either side; it does not twist.
fn tail_limits() -> Result<Limits, FigureError> {
    Ok(Limits::new(
        Limit::new(-0.60, 1.10)?,
        Limit::FIXED,
        Limit::symmetric(0.80)?,
    ))
}

fn push_joint(joints: &mut ArrayVec<Joint, MAX_JOINTS>, joint: Joint) -> Result<(), FigureError> {
    joints
        .try_push(joint)
        .map_err(|_| FigureError::TooManyJoints)
}

fn push_part(parts: &mut ArrayVec<Part, MAX_PARTS>, part: Part) -> Result<(), FigureError> {
    parts.try_push(part).map_err(|_| FigureError::TooManyParts)
}

fn mount(
    mounts: &mut ArrayVec<(Socket, Mount), { Socket::COUNT }>,
    socket: Socket,
    bone: Bone,
    at: Body,
    scale: f64,
) -> Result<(), FigureError> {
    mounts
        .try_push((socket, Mount::new(bone.joint(), at).scaled(scale)))
        .map_err(|_| FigureError::DuplicateSocket)
}

/// How many drives bind the pose parameters to this rig.
pub const DRIVE_COUNT: usize = 30;

/// The parameters that move the trunk, the head and the arms.
///
/// The spine belongs here rather than with the legs, which is what lets a
/// cast play over a walk: the upper body leans into the cast while the legs
/// keep their own clip.
pub const UPPER_BODY: Mask = Mask::NONE
    .with(Param::SpineBend)
    .with(Param::SpineTwist)
    .with(Param::SpineTilt)
    .with(Param::HeadTurn)
    .with(Param::HeadNod)
    .with(Param::HeadTilt)
    .with(Param::ShoulderSwing(Side::Left))
    .with(Param::ShoulderSwing(Side::Right))
    .with(Param::ShoulderSplay(Side::Left))
    .with(Param::ShoulderSplay(Side::Right))
    .with(Param::ElbowBend(Side::Left))
    .with(Param::ElbowBend(Side::Right))
    .with(Param::WristAngle(Side::Left))
    .with(Param::WristAngle(Side::Right));

/// The parameters that move the legs and the tail, which the locomotion
/// under an upper-body clip keeps.
pub const LOWER_BODY: Mask = Mask::ALL.difference(UPPER_BODY);

/// Which joint axis each pose parameter turns on this rig.
///
/// Every sense in this table is a fact about the body rather than a
/// convention: a limb hangs below its joint, so it swings opposite to the
/// joint's own top, and an outward splay is a different sign on each side.
/// Stating both here once is what lets a clip say "swing the arm forward" or
/// "lift it away from the body" and be right on both sides.
pub const DRIVES: [Drive; DRIVE_COUNT] = [
    Drive::new(Param::SpineBend, Bone::Waist.joint(), Axis::Pitch),
    Drive::new(Param::SpineBend, Bone::Chest.joint(), Axis::Pitch),
    Drive::new(Param::SpineTwist, Bone::Waist.joint(), Axis::Yaw),
    Drive::new(Param::SpineTwist, Bone::Chest.joint(), Axis::Yaw),
    Drive::new(Param::SpineTilt, Bone::Waist.joint(), Axis::Roll),
    Drive::new(Param::SpineTilt, Bone::Chest.joint(), Axis::Roll),
    Drive::new(Param::HeadNod, Bone::Neck.joint(), Axis::Pitch),
    Drive::new(Param::HeadNod, Bone::Head.joint(), Axis::Pitch),
    Drive::new(Param::HeadTurn, Bone::Neck.joint(), Axis::Yaw),
    Drive::new(Param::HeadTurn, Bone::Head.joint(), Axis::Yaw),
    Drive::new(Param::HeadTilt, Bone::Neck.joint(), Axis::Roll),
    Drive::new(Param::HeadTilt, Bone::Head.joint(), Axis::Roll),
    arm_swing(Side::Left),
    arm_swing(Side::Right),
    arm_splay(Side::Left),
    arm_splay(Side::Right),
    elbow(Side::Left),
    elbow(Side::Right),
    wrist(Side::Left),
    wrist(Side::Right),
    leg_swing(Side::Left),
    leg_swing(Side::Right),
    leg_splay(Side::Left),
    leg_splay(Side::Right),
    knee(Side::Left),
    knee(Side::Right),
    ankle(Side::Left),
    ankle(Side::Right),
    // A tail hangs down its frame like a limb, so a positive pitch swings it
    // back and up, and a positive roll swings it to the figure's left.
    Drive::new(Param::TailLift, Bone::Tail.joint(), Axis::Pitch),
    Drive::new(Param::TailSwing, Bone::Tail.joint(), Axis::Roll),
];

/// Forward is a negative pitch, because the arm hangs below the shoulder.
const fn arm_swing(side: Side) -> Drive {
    Drive::new(
        Param::ShoulderSwing(side),
        Bone::Shoulder(side).joint(),
        Axis::Pitch,
    )
    .reversed()
}

/// Away from the body, which is a positive roll on the left and a negative
/// one on the right.
const fn arm_splay(side: Side) -> Drive {
    let drive = Drive::new(
        Param::ShoulderSplay(side),
        Bone::Shoulder(side).joint(),
        Axis::Roll,
    );
    match side {
        Side::Left => drive,
        Side::Right => drive.reversed(),
    }
}

/// An elbow closes toward a negative pitch and cannot open past straight.
const fn elbow(side: Side) -> Drive {
    Drive::new(
        Param::ElbowBend(side),
        Bone::Elbow(side).joint(),
        Axis::Pitch,
    )
    .reversed()
}

const fn wrist(side: Side) -> Drive {
    Drive::new(
        Param::WristAngle(side),
        Bone::Wrist(side).joint(),
        Axis::Pitch,
    )
}

/// As the arm: forward is a negative pitch.
const fn leg_swing(side: Side) -> Drive {
    Drive::new(Param::HipSwing(side), Bone::Hip(side).joint(), Axis::Pitch).reversed()
}

/// As the arm: outward is handed.
const fn leg_splay(side: Side) -> Drive {
    let drive = Drive::new(Param::HipSplay(side), Bone::Hip(side).joint(), Axis::Roll);
    match side {
        Side::Left => drive,
        Side::Right => drive.reversed(),
    }
}

/// A knee folds toward a positive pitch, the heel rising behind.
const fn knee(side: Side) -> Drive {
    Drive::new(Param::KneeBend(side), Bone::Knee(side).joint(), Axis::Pitch)
}

const fn ankle(side: Side) -> Drive {
    Drive::new(
        Param::AnkleAngle(side),
        Bone::Ankle(side).joint(),
        Axis::Pitch,
    )
}

/// The humanoid's two legs, for the planting solve.
///
/// Named here because a leg is the rig's own anatomy: the solve knows what a
/// two-bone chain is without knowing what a humanoid is, so which joints
/// those are is stated once, beside the skeleton that has them.
#[must_use]
pub fn legs() -> [Leg; 2] {
    [leg(Side::Left), leg(Side::Right)]
}

const fn leg(side: Side) -> Leg {
    Leg {
        hip: Bone::Hip(side).joint(),
        knee: Bone::Knee(side).joint(),
        ankle: Bone::Ankle(side).joint(),
        swing: Param::HipSwing(side),
        splay: Param::HipSplay(side),
        bend: Param::KneeBend(side),
        flex: Param::AnkleAngle(side),
    }
}

/// `rig` with the humanoid pose parameters bound to its joints.
///
/// # Errors
///
/// As [`Rigging::new`]; cannot fail for a rig [`rig`] built, whose joints are
/// exactly the ones [`DRIVES`] names.
///
/// [`rig`]: fn@rig
pub fn rigging(rig: &Rig) -> Result<Rigging<'_>, FigureError> {
    Rigging::new(rig, &DRIVES)
}

#[cfg(test)]
#[path = "humanoid/tests.rs"]
mod tests;
