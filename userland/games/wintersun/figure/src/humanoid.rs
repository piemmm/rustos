//! The humanoid rig: the first figure the engine draws.
//!
//! Proportioned in percentages of [`STANDING_HEIGHT`], so every offset below
//! reads directly as a fraction of the figure's own height and a reviewer can
//! check a shoulder sits at 82% without converting anything.
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

use crate::error::FigureError;
use crate::frame::Body;
use crate::joint::{Joint, JointId, Limit, Limits, MAX_JOINTS};
use crate::mesh::Ring;
use crate::plant::Leg;
use crate::pose::{Mask, Param};
use crate::rig::{Part, Rig, MAX_PARTS};
use crate::rigging::{Axis, Drive, Rigging};
use crate::socket::{Mount, Side, Socket};

/// How tall the figure stands, in the figure-local pixels the rig is authored
/// in.
///
/// A hundred, so an offset is a percentage of height. A caller draws the
/// figure at any size by scaling: the factor is the height it wants over
/// this.
pub const STANDING_HEIGHT: f64 = 100.0;

/// How many joints the humanoid has.
pub const JOINT_COUNT: usize = 17;

/// How many parts it is drawn from.
pub const PART_COUNT: usize = 21;

const _: () = assert!(JOINT_COUNT <= MAX_JOINTS);
const _: () = assert!(PART_COUNT <= MAX_PARTS);

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
        })
    }

    /// Its position in the rig's joint table.
    #[must_use]
    pub const fn index(self) -> usize {
        self.joint().index()
    }
}

/// The tones the figure is drawn in.
///
/// Named once here rather than as literals at each part, so a re-tint is a
/// change to this block.
pub mod palette {
    use tairix_raster::Color;

    /// Lit skin: the brow and the back of a hand.
    pub const SKIN_LIT: Color = Color::rgb(0xE8, 0xBC, 0x98);
    /// Mid skin: the face, the neck, a bare arm.
    pub const SKIN_MID: Color = Color::rgb(0xCE, 0x9E, 0x78);
    /// Skin turned away from the light.
    pub const SKIN_SHADE: Color = Color::rgb(0xA8, 0x7A, 0x58);
    /// The lit face of the tunic.
    pub const CLOTH_LIT: Color = Color::rgb(0x4C, 0x5A, 0x6E);
    /// The tunic in shadow, and the trousers.
    pub const CLOTH_SHADE: Color = Color::rgb(0x33, 0x3E, 0x4E);
    /// Boots and belt leather.
    pub const LEATHER: Color = Color::rgb(0x4A, 0x36, 0x24);

    /// Every tone the figure is drawn in.
    ///
    /// The list a conformance check reads, so "on palette" is a membership
    /// test rather than an eye. A test holds it against the rig's own parts,
    /// so a tone added above and left out here fails rather than escaping
    /// the check.
    pub const ALL: [Color; 6] = [
        SKIN_LIT,
        SKIN_MID,
        SKIN_SHADE,
        CLOTH_LIT,
        CLOTH_SHADE,
        LEATHER,
    ];
}

/// Assemble the humanoid rig.
///
/// Built rather than a constant because the joint limits are checked
/// intervals and the two sides are mirrored from one table. A caller builds
/// it once and holds it: a rig does not change between frames, and validating
/// one per frame would be work paid for nothing.
///
/// # Errors
///
/// Only what [`Rig::new`] refuses, and only if this table is edited into
/// something inconsistent — which is the point of checking it here.
///
/// [`Rig::new`]: crate::rig::Rig::new
pub fn rig() -> Result<Rig, FigureError> {
    let mut joints: ArrayVec<Joint, MAX_JOINTS> = ArrayVec::new();
    let mut parts: ArrayVec<Part, MAX_PARTS> = ArrayVec::new();
    let mut mounts: ArrayVec<(Socket, Mount), { Socket::COUNT }> = ArrayVec::new();

    spine(&mut joints)?;
    arms(&mut joints)?;
    leg_joints(&mut joints)?;
    trunk(&mut parts)?;
    limbs(&mut parts)?;
    sockets(&mut mounts)?;

    Rig::new(&joints, &parts, &mounts)
}

/// The spine, bottom to top. Each joint's offset is the length of the
/// segment below it, so moving one moves everything it carries.
fn spine(joints: &mut ArrayVec<Joint, MAX_JOINTS>) -> Result<(), FigureError> {
    push_joint(
        joints,
        Joint::new(None, Body::new(0.0, 0.0, PELVIS_HEIGHT), spine_limits()?),
    )?;
    push_joint(
        joints,
        Joint::new(
            Some(Bone::Pelvis.joint()),
            Body::new(0.0, 0.0, 9.0),
            spine_limits()?,
        ),
    )?;
    push_joint(
        joints,
        Joint::new(
            Some(Bone::Waist.joint()),
            Body::new(0.0, 0.0, 13.0),
            spine_limits()?,
        ),
    )?;
    push_joint(
        joints,
        Joint::new(
            Some(Bone::Chest.joint()),
            Body::new(0.0, 0.0, 10.0),
            neck_limits()?,
        ),
    )?;
    push_joint(
        joints,
        Joint::new(
            Some(Bone::Neck.joint()),
            Body::new(0.0, 0.0, 4.0),
            skull_limits()?,
        ),
    )?;

    Ok(())
}

/// Both arms, mirrored from one pass.
fn arms(joints: &mut ArrayVec<Joint, MAX_JOINTS>) -> Result<(), FigureError> {
    for side in Side::BOTH {
        let across = side.across();
        push_joint(
            joints,
            Joint::new(
                Some(Bone::Chest.joint()),
                Body::new(0.0, across * 11.5, 8.0),
                shoulder_limits(side)?,
            ),
        )?;
        push_joint(
            joints,
            Joint::new(
                Some(Bone::Shoulder(side).joint()),
                Body::new(0.0, 0.0, -UPPER_ARM_LENGTH),
                elbow_limits()?,
            ),
        )?;
        push_joint(
            joints,
            Joint::new(
                Some(Bone::Elbow(side).joint()),
                Body::new(0.0, 0.0, -FOREARM_LENGTH),
                wrist_limits()?,
            ),
        )?;
    }

    Ok(())
}

/// Both legs, mirrored from one pass.
fn leg_joints(joints: &mut ArrayVec<Joint, MAX_JOINTS>) -> Result<(), FigureError> {
    for side in Side::BOTH {
        let across = side.across();
        push_joint(
            joints,
            Joint::new(
                Some(Bone::Pelvis.joint()),
                Body::new(0.0, across * 9.0, -1.0),
                hip_limits(side)?,
            ),
        )?;
        push_joint(
            joints,
            Joint::new(
                Some(Bone::Hip(side).joint()),
                Body::new(0.0, 0.0, -THIGH_LENGTH),
                knee_limits()?,
            ),
        )?;
        push_joint(
            joints,
            Joint::new(
                Some(Bone::Knee(side).joint()),
                Body::new(0.0, 0.0, -SHANK_LENGTH),
                ankle_limits()?,
            ),
        )?;
    }

    Ok(())
}

/// A ring at `up` above the part's origin, `wide` across and `deep`
/// through.
const fn hoop(up: f64, wide: f64, deep: f64) -> Ring {
    Ring::new(Body::new(0.0, 0.0, up), wide, deep)
}

/// The trunk, bottom to top. Each part's upper rings are carried by the
/// joint above it, so the torso bends through the waist and the chest
/// rather than telescoping at them.
fn trunk(parts: &mut ArrayVec<Part, MAX_PARTS>) -> Result<(), FigureError> {
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
    // Every free end is closed: a tube left open shows its own near rim as
    // a crescent where the surface should have ended, which at the crown of
    // a head reads as a notch cut out of it.
    const SKULL: [Ring; 6] = [
        hoop(-1.0, 2.6, 2.8),
        hoop(1.5, 4.4, 4.8),
        hoop(4.5, 5.0, 5.5),
        hoop(8.0, 4.6, 5.2),
        hoop(10.5, 3.2, 3.6),
        hoop(12.0, 0.9, 1.0),
    ];

    push_part(
        parts,
        Part::new(
            Bone::Pelvis.joint(),
            Body::ORIGIN,
            &PELVIS,
            palette::CLOTH_SHADE,
        )?
        .spanning(Bone::Waist.joint()),
    )?;
    push_part(
        parts,
        Part::new(
            Bone::Waist.joint(),
            Body::ORIGIN,
            &WAIST,
            palette::CLOTH_LIT,
        )?
        .spanning(Bone::Chest.joint()),
    )?;
    push_part(
        parts,
        Part::new(
            Bone::Chest.joint(),
            Body::ORIGIN,
            &CHEST,
            palette::CLOTH_LIT,
        )?
        .spanning(Bone::Neck.joint()),
    )?;
    push_part(
        parts,
        Part::new(Bone::Neck.joint(), Body::ORIGIN, &NECK, palette::SKIN_SHADE)?
            .spanning(Bone::Head.joint()),
    )?;
    push_part(
        parts,
        Part::new(Bone::Head.joint(), Body::ORIGIN, &SKULL, palette::SKIN_MID)?,
    )?;

    Ok(())
}

/// The arms and the legs, each mirrored from one pass.
fn limbs(parts: &mut ArrayVec<Part, MAX_PARTS>) -> Result<(), FigureError> {
    arm_parts(parts)?;
    leg_parts(parts)
}

/// Both arms, and the sleeve each swings out of.
fn arm_parts(parts: &mut ArrayVec<Part, MAX_PARTS>) -> Result<(), FigureError> {
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
    for side in Side::BOTH {
        push_part(
            parts,
            Part::new(
                Bone::Shoulder(side).joint(),
                Body::ORIGIN,
                &SLEEVE,
                palette::CLOTH_LIT,
            )?,
        )?;
        push_part(
            parts,
            Part::new(
                Bone::Shoulder(side).joint(),
                Body::ORIGIN,
                &UPPER_ARM,
                palette::SKIN_MID,
            )?
            .spanning(Bone::Elbow(side).joint()),
        )?;
        push_part(
            parts,
            Part::new(
                Bone::Elbow(side).joint(),
                Body::ORIGIN,
                &FOREARM,
                palette::SKIN_MID,
            )?
            .spanning(Bone::Wrist(side).joint()),
        )?;
        push_part(
            parts,
            Part::new(
                Bone::Wrist(side).joint(),
                Body::ORIGIN,
                &HAND,
                palette::SKIN_LIT,
            )?,
        )?;
    }

    Ok(())
}

/// Both legs and their boots, mirrored from one pass.
fn leg_parts(parts: &mut ArrayVec<Part, MAX_PARTS>) -> Result<(), FigureError> {
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
    // A foot runs forward from its ankle rather than hanging below it, and
    // is about a seventh of a person's height: drawn much shorter it stops
    // reading as a foot at all at icon size.
    const FOOT: [Ring; 5] = [
        Ring::new(Body::new(-3.4, 0.0, 0.3), 1.2, 1.2),
        Ring::new(Body::new(-2.0, 0.0, 0.0), 2.6, 2.3),
        Ring::new(Body::new(2.5, 0.0, -0.4), 2.8, 2.4),
        Ring::new(Body::new(6.8, 0.0, -0.8), 2.3, 1.9),
        Ring::new(Body::new(9.4, 0.0, -1.0), 0.9, 0.8),
    ];

    for side in Side::BOTH {
        let across = side.across();
        push_part(
            parts,
            Part::new(
                Bone::Hip(side).joint(),
                Body::ORIGIN,
                &HAUNCH,
                palette::CLOTH_SHADE,
            )?,
        )?;
        push_part(
            parts,
            Part::new(
                Bone::Hip(side).joint(),
                Body::ORIGIN,
                &THIGH,
                palette::CLOTH_SHADE,
            )?
            .spanning(Bone::Knee(side).joint()),
        )?;
        push_part(
            parts,
            Part::new(
                Bone::Knee(side).joint(),
                Body::ORIGIN,
                &SHANK,
                palette::CLOTH_SHADE,
            )?
            .spanning(Bone::Ankle(side).joint()),
        )?;
        push_part(
            parts,
            Part::new(
                Bone::Ankle(side).joint(),
                Body::new(0.0, across * 0.3, -2.2),
                &FOOT,
                palette::LEATHER,
            )?,
        )?;
    }

    Ok(())
}

/// Where equipment hangs.
fn sockets(mounts: &mut ArrayVec<(Socket, Mount), { Socket::COUNT }>) -> Result<(), FigureError> {
    mount(
        mounts,
        Socket::Head,
        Bone::Head,
        Body::new(0.0, 0.0, SKULL_RISE),
    )?;
    mount(mounts, Socket::Back, Bone::Chest, Body::new(-6.0, 0.0, 6.0))?;
    mount(
        mounts,
        Socket::MainHand,
        Bone::Wrist(Side::Right),
        Body::new(2.0, 0.0, -3.4),
    )?;
    mount(
        mounts,
        Socket::OffHand,
        Bone::Wrist(Side::Left),
        Body::new(2.0, 0.0, -3.4),
    )?;
    for side in Side::BOTH {
        let across = side.across();
        mount(
            mounts,
            Socket::Shoulder(side),
            Bone::Shoulder(side),
            Body::new(0.0, across * 1.6, 2.0),
        )?;
        mount(
            mounts,
            Socket::Hip(side),
            Bone::Hip(side),
            Body::new(0.0, across * 2.0, 2.0),
        )?;
        mount(
            mounts,
            Socket::Foot(side),
            Bone::Ankle(side),
            Body::new(1.6, 0.0, -1.2),
        )?;
    }

    Ok(())
}

/// Where the pelvis sits above the ground.
const PELVIS_HEIGHT: f64 = 52.0;

/// How far the skull's centre sits above the atlas.
const SKULL_RISE: f64 = 6.0;

/// Shoulder to elbow.
const UPPER_ARM_LENGTH: f64 = 19.0;

/// Elbow to wrist.
const FOREARM_LENGTH: f64 = 15.0;

/// Hip to knee.
pub(crate) const THIGH_LENGTH: f64 = 23.0;

/// Knee to ankle.
pub(crate) const SHANK_LENGTH: f64 = 24.0;

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
) -> Result<(), FigureError> {
    mounts
        .try_push((socket, Mount::new(bone.joint(), at)))
        .map_err(|_| FigureError::DuplicateSocket)
}

/// How many drives bind the pose parameters to this rig.
pub const DRIVE_COUNT: usize = 28;

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

/// The parameters that move the legs.
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
