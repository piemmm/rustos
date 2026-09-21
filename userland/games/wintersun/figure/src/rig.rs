//! The rig: a skeleton, the parts it carries, the sockets gear hangs on, and
//! the pass that turns a posture into painted shapes.

use tairix_inline::ArrayVec;
use tairix_raster::shape::{Placed, Shape};
use tairix_raster::Color;
use tairix_util::mathf;
use tairix_wintersun_net::value::Facing;

use crate::error::FigureError;
use crate::frame::{project, screen_turn, Basis, Body, Rotation};
use crate::joint::{Joint, JointId, MAX_JOINTS};
use crate::socket::{Mount, Socket};

/// How many parts one rig is built from.
///
/// A bound on authored content, like the joint bound: the shipped rigs are
/// held to it at build time, and a figure that wants more parts is a
/// different figure rather than a bigger one.
pub const MAX_PARTS: usize = 96;

/// How many equipment parts one figure carries at once.
pub const MAX_FITTED: usize = 24;

/// How many shapes one placed figure amounts to.
pub const MAX_PLACED: usize = MAX_PARTS + MAX_FITTED;

const _: () = assert!(MAX_PLACED <= u16::MAX as usize);

/// One shape of the figure's own body, bound to the joint that carries it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Part {
    /// The joint it rides.
    ///
    /// Naming the joint rather than a position is the whole of the
    /// joint-carries-mass rule: a shoulder cap and the arm that swings from
    /// it are the same joint's, so they cannot drift apart.
    pub joint: JointId,
    /// Where it sits in that joint's frame.
    pub at: Body,
    /// What it is drawn as.
    pub shape: Shape,
    /// Its colour.
    pub color: Color,
}

impl Part {
    /// A part on `joint` at `at`.
    #[must_use]
    pub const fn new(joint: JointId, at: Body, shape: Shape, color: Color) -> Self {
        Self {
            joint,
            at,
            shape,
            color,
        }
    }
}

/// One shape of a piece of equipment, hung on a socket.
///
/// Stated against a socket rather than a joint, so one helm fits every rig
/// that offers a head and no gear knows a skeleton.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Fitted {
    /// The socket it hangs on.
    pub socket: Socket,
    /// Where it sits in that socket's frame.
    pub at: Body,
    /// What it is drawn as.
    pub shape: Shape,
    /// Its colour.
    pub color: Color,
    /// How far it leans from its socket's rest.
    pub turn: Rotation,
}

impl Fitted {
    /// A fitted shape on `socket` at `at`, resting as the socket does.
    #[must_use]
    pub const fn new(socket: Socket, at: Body, shape: Shape, color: Color) -> Self {
        Self {
            socket,
            at,
            shape,
            color,
            turn: Rotation::REST,
        }
    }

    /// The same shape turned `turn` from its socket's own rest.
    ///
    /// What a sway layer writes: a cloak trails the turn by leaning against
    /// the socket rather than by the socket moving, so the rig's statement of
    /// where gear rests stays the rig's.
    #[must_use]
    pub const fn turned(mut self, turn: Rotation) -> Self {
        self.turn = turn;
        self
    }
}

/// A rigid frame: where something ended up and how it is turned.
///
/// One type for a resolved joint and for the figure's own root, because they
/// are the same thing at different depths — the root is the frame the
/// parentless joints hang in, so a whole-figure lift or tilt costs the resolve
/// nothing beyond the value it already inherits.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Resolved {
    /// Where it sits in the frame it is held in.
    pub at: Body,
    /// How it is turned there.
    pub basis: Basis,
}

impl Resolved {
    /// The origin, unturned.
    pub const REST: Self = Self {
        at: Body::ORIGIN,
        basis: Basis::IDENTITY,
    };

    /// A root displaced by `offset` and tilted by `tilt`.
    ///
    /// The tilt pivots about the frame's origin, which for a figure's root is
    /// the ground point its feet rest on — so a figure leaning into a slope
    /// turns about its contact with it rather than swinging its feet through
    /// it. No limit binds it: a slope is the world's angle, not a joint's.
    ///
    /// # Errors
    ///
    /// [`FigureError::GeometryUnreal`] for an offset or an angle that is not
    /// finite.
    pub fn rooted(offset: Body, tilt: Rotation) -> Result<Self, FigureError> {
        if !offset.is_real() || !tilt.is_real() {
            return Err(FigureError::GeometryUnreal);
        }
        Ok(Self {
            at: offset,
            basis: Basis::of(tilt),
        })
    }
}

/// Every joint's resolved frame, for one posture in one root.
///
/// Held by the caller across frames like [`Placement`], so asking where a
/// figure's feet ended up costs no allocation.
#[derive(Clone, Debug)]
pub struct Frames {
    frames: [Resolved; MAX_JOINTS],
    len: usize,
}

impl Frames {
    /// Nothing resolved yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            frames: [Resolved::REST; MAX_JOINTS],
            len: 0,
        }
    }

    /// Where `joint` ended up, or `None` if the resolve did not cover it.
    #[must_use]
    pub fn get(&self, joint: JointId) -> Option<Resolved> {
        if joint.index() < self.len {
            Some(self.frames[joint.index()])
        } else {
            None
        }
    }
}

impl Default for Frames {
    fn default() -> Self {
        Self::new()
    }
}

/// A skeleton, its parts, and the sockets it offers.
#[derive(Clone, Debug)]
pub struct Rig {
    joints: ArrayVec<Joint, MAX_JOINTS>,
    parts: ArrayVec<Part, MAX_PARTS>,
    mounts: [Option<Mount>; Socket::COUNT],
    reach: f64,
}

impl Rig {
    /// Assemble and check a rig.
    ///
    /// Every refusal below is a rig that could only draw something nobody
    /// authored, so it is caught here rather than on a frame.
    ///
    /// # Errors
    ///
    /// [`FigureError::TooManyJoints`] / [`FigureError::TooManyParts`] beyond
    /// what a rig holds; [`FigureError::NoSuchParent`] and
    /// [`FigureError::ParentNotEarlier`] for a hierarchy that is not a
    /// parents-first forest; [`FigureError::NoSuchJoint`] for a part or mount
    /// naming a joint that does not exist; [`FigureError::DuplicateSocket`]
    /// for a second mount on one socket; [`FigureError::GeometryUnreal`] for
    /// a dimension that is not finite; and the two that carry the visual
    /// rule — [`FigureError::BearingJointWithoutMass`] for a joint that
    /// bears a child but draws nothing itself, and
    /// [`FigureError::JointBeyondParentReach`] for a child whose origin lies
    /// outside everything its parent draws.
    pub fn new(
        joints: &[Joint],
        parts: &[Part],
        mounts: &[(Socket, Mount)],
    ) -> Result<Self, FigureError> {
        let mut rig = Self {
            joints: ArrayVec::new(),
            parts: ArrayVec::new(),
            mounts: [None; Socket::COUNT],
            reach: 0.0,
        };

        for (index, joint) in joints.iter().enumerate() {
            if !joint.at.is_real() || !joint.orientation.is_real() {
                return Err(FigureError::GeometryUnreal);
            }
            if let Some(parent) = joint.parent {
                if parent.index() >= joints.len() {
                    return Err(FigureError::NoSuchParent);
                }
                if parent.index() >= index {
                    return Err(FigureError::ParentNotEarlier);
                }
            }
            rig.joints
                .try_push(*joint)
                .map_err(|_| FigureError::TooManyJoints)?;
        }

        for part in parts {
            if part.joint.index() >= rig.joints.len() {
                return Err(FigureError::NoSuchJoint);
            }
            if !part.at.is_real() || !part.shape.is_real() {
                return Err(FigureError::GeometryUnreal);
            }
            rig.parts
                .try_push(*part)
                .map_err(|_| FigureError::TooManyParts)?;
        }

        for (socket, mount) in mounts {
            if mount.joint.index() >= rig.joints.len() {
                return Err(FigureError::NoSuchJoint);
            }
            if !mount.at.is_real() || !mount.orientation.is_real() {
                return Err(FigureError::GeometryUnreal);
            }
            let slot = &mut rig.mounts[socket.index()];
            if slot.is_some() {
                return Err(FigureError::DuplicateSocket);
            }
            *slot = Some(*mount);
        }

        rig.check_bearing_joints()?;
        rig.reach = rig.rest_reach();
        Ok(rig)
    }

    /// Its joints, parents first.
    #[must_use]
    pub fn joints(&self) -> &[Joint] {
        &self.joints
    }

    /// Its own parts, in the order a depth tie paints them.
    #[must_use]
    pub fn parts(&self) -> &[Part] {
        &self.parts
    }

    /// Where it mounts `socket`, or `None` if it does not offer one.
    #[must_use]
    pub fn mount(&self, socket: Socket) -> Option<Mount> {
        self.mounts[socket.index()]
    }

    /// The furthest any part reaches from the figure's ground point at rest,
    /// in the figure-local pixels the rig is authored in.
    ///
    /// Read off the rig rather than asserted about it, so a caller sizes the
    /// surface a figure must fit inside from the figure itself.
    #[must_use]
    pub fn reach(&self) -> f64 {
        self.reach
    }

    /// Every joint that bears a child carries a part of its own, and no
    /// child's origin lies outside everything its parent draws.
    ///
    /// The second test is one-sided on purpose: a shape's reach is an outer
    /// bound, so exceeding it proves a gap while clearing it does not prove a
    /// seam. Proving the seam is a measurement over rendered pixels, which
    /// the art harness makes; this catches the limb that is nowhere near its
    /// socket, which is the defect that actually gets authored.
    fn check_bearing_joints(&self) -> Result<(), FigureError> {
        for joint in &self.joints {
            let Some(parent) = joint.parent else { continue };
            let mut covered: Option<f64> = None;
            for part in &self.parts {
                if part.joint == parent {
                    let bound = part.at.length() + part.shape.reach();
                    covered = Some(covered.map_or(bound, |best| mathf::fmax(best, bound)));
                }
            }
            let Some(covered) = covered else {
                return Err(FigureError::BearingJointWithoutMass);
            };
            if joint.at.length() > covered {
                return Err(FigureError::JointBeyondParentReach);
            }
        }
        Ok(())
    }

    /// The furthest a part's own extent reaches from the ground point, with
    /// every joint at rest.
    fn rest_reach(&self) -> f64 {
        let mut frames = Frames::new();
        let rest = [Rotation::REST; MAX_JOINTS];
        self.resolve(&rest[..self.joints.len()], Resolved::REST, &mut frames);
        let mut furthest = 0.0;
        for part in &self.parts {
            let frame = frames.frames[part.joint.index()];
            let at = frame.at.plus(frame.basis.apply(part.at));
            furthest = mathf::fmax(furthest, at.length() + part.shape.reach());
        }
        furthest
    }

    /// Resolve every joint's frame in one forward pass.
    ///
    /// A parent always precedes its child, so a parent's frame is already
    /// written when its child reads it. `rotations` comes only from a
    /// [`Posture`] built for this rig, so it is exactly as long as the joint
    /// table and the walk visits every joint.
    fn resolve(&self, rotations: &[Rotation], root: Resolved, out: &mut Frames) {
        for (index, (joint, rotation)) in self.joints.iter().zip(rotations).enumerate() {
            let carried = joint
                .parent
                .map_or(root, |parent| out.frames[parent.index()]);
            let local = Basis::of(joint.orientation).compose(Basis::of(*rotation));
            out.frames[index] = Resolved {
                at: carried.at.plus(carried.basis.apply(joint.at)),
                basis: carried.basis.compose(local),
            };
        }
        out.len = self.joints.len();
    }
}

/// A rig with every joint turned somewhere it is allowed to be.
///
/// Borrows its rig, so a posture can only ever be placed on the rig whose
/// limits admitted it — there is no pairing to get wrong and [`place`] has
/// no out-of-limit case to handle.
///
/// [`place`]: Self::place
#[derive(Copy, Clone, Debug)]
pub struct Posture<'a> {
    rig: &'a Rig,
    /// One slot per joint the rig *could* hold, so building a posture has no
    /// failure to report and the slots beyond the rig's own joints are simply
    /// never read.
    rotations: [Rotation; MAX_JOINTS],
}

impl<'a> Posture<'a> {
    /// `rig` with every joint at rest.
    #[must_use]
    pub const fn rest(rig: &'a Rig) -> Self {
        Self {
            rig,
            rotations: [Rotation::REST; MAX_JOINTS],
        }
    }

    /// The rig it poses.
    #[must_use]
    pub fn rig(&self) -> &'a Rig {
        self.rig
    }

    /// How far `joint` is turned, or `None` if the rig has no such joint.
    #[must_use]
    pub fn get(&self, joint: JointId) -> Option<Rotation> {
        if joint.index() < self.rig.joints.len() {
            Some(self.rotations[joint.index()])
        } else {
            None
        }
    }

    /// Turn `joint` to `rotation`.
    ///
    /// # Errors
    ///
    /// [`FigureError::NoSuchJoint`] if the rig has no such joint, and
    /// [`FigureError::RotationOutsideLimit`] if the rotation is outside what
    /// that joint documents — which is where an animation that would bend an
    /// elbow backwards fails, rather than on the frame that drew it.
    pub fn set(&mut self, joint: JointId, rotation: Rotation) -> Result<(), FigureError> {
        let held = self
            .rig
            .joints
            .get(joint.index())
            .ok_or(FigureError::NoSuchJoint)?;
        if !held.limits.holds(rotation) {
            return Err(FigureError::RotationOutsideLimit);
        }
        // A `JointId` cannot exceed `MAX_JOINTS`, which is this array's own
        // length, so the slot exists.
        self.rotations[joint.index()] = rotation;
        Ok(())
    }

    /// Resolve every joint's frame for this posture, hanging in `root`.
    ///
    /// What a layer that needs to know *where* the figure is reads — which
    /// foot is forward, how far a hand has reached — before deciding
    /// anything. Placing does the same resolve, so a caller that only paints
    /// never pays for this one.
    pub fn resolve(&self, root: Resolved, out: &mut Frames) {
        self.rig.resolve(&self.rotations, root, out);
    }

    /// Place the figure for a stance.
    ///
    /// `stance` carries the heading, the figure-local-to-surface scale, the
    /// surface point the figure's ground contact sits on, and the root
    /// transform a lift, a crouch or a slope tilt is expressed as. `fitted`
    /// is the equipment hung on its sockets.
    ///
    /// The result is sorted far-first, so painting it in order composites the
    /// figure correctly: a depth tie breaks on the rig's own part order, and
    /// equipment follows the body, so a piece of piping stays behind the flap
    /// it edges and a pauldron stays in front of the shoulder it covers.
    ///
    /// # Errors
    ///
    /// [`FigureError::GeometryUnreal`] for a fitted dimension that is not
    /// finite, [`FigureError::NoSuchSocket`] for equipment naming a socket
    /// this rig does not offer, and [`FigureError::TooMuchEquipment`] for
    /// more gear than a figure carries. The stance's own values were checked
    /// where it was built.
    pub fn place(
        &self,
        stance: &Stance,
        fitted: &[Fitted],
        out: &mut Placement,
    ) -> Result<(), FigureError> {
        out.len = 0;
        // The rig's own parts are bounded by `MAX_PARTS` and the buffer is
        // their sum with `MAX_FITTED`, so this one check is what makes every
        // slot below exist.
        if fitted.len() > MAX_FITTED {
            return Err(FigureError::TooMuchEquipment);
        }

        self.rig
            .resolve(&self.rotations, stance.root, &mut out.frames);

        for part in &self.rig.parts {
            let frame = out.frames.frames[part.joint.index()];
            out.push(stance, frame, part.at, part.shape, part.color);
        }
        for piece in fitted {
            let mount = self
                .rig
                .mount(piece.socket)
                .ok_or(FigureError::NoSuchSocket)?;
            if !piece.at.is_real() || !piece.shape.is_real() || !piece.turn.is_real() {
                return Err(FigureError::GeometryUnreal);
            }
            let carried = out.frames.frames[mount.joint.index()];
            let frame = Resolved {
                at: carried.at.plus(carried.basis.apply(mount.at)),
                basis: carried
                    .basis
                    .compose(Basis::of(mount.orientation).compose(Basis::of(piece.turn))),
            };
            out.push(stance, frame, piece.at, piece.shape, piece.color);
        }

        out.sort();
        Ok(())
    }
}

/// Where a figure stands, and how its whole body is displaced there.
///
/// Gathers what turning a rig into surface pixels needs: the heading, the
/// scale, the surface ground point, and the root transform. Validated once
/// here, so placing has nothing left to check about it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Stance {
    facing: Facing,
    scale: f64,
    at: (f64, f64),
    root: Resolved,
}

impl Stance {
    /// A figure facing `facing`, drawn at `scale`, standing at `at`.
    ///
    /// `scale` converts the figure-local pixels the rig is authored in to
    /// surface pixels — the figure's drawn height over [`Rig::reach`]'s own
    /// units — and `at` is the surface column and row its feet rest on. The
    /// root starts at rest; [`Self::rooted`] displaces it.
    ///
    /// # Errors
    ///
    /// [`FigureError::ScaleUnreal`] for a scale that is not finite and
    /// positive, and [`FigureError::GeometryUnreal`] for a ground point that
    /// is not finite.
    pub fn new(facing: Facing, scale: f64, at: (f64, f64)) -> Result<Self, FigureError> {
        if !scale.is_finite() || scale <= 0.0 {
            return Err(FigureError::ScaleUnreal);
        }
        if !at.0.is_finite() || !at.1.is_finite() {
            return Err(FigureError::GeometryUnreal);
        }
        Ok(Self {
            facing,
            scale,
            at,
            root: Resolved::REST,
        })
    }

    /// The same stance with the whole figure displaced and tilted by `root`.
    #[must_use]
    pub const fn rooted(mut self, root: Resolved) -> Self {
        self.root = root;
        self
    }

    /// The heading it faces.
    #[must_use]
    pub const fn facing(&self) -> Facing {
        self.facing
    }

    /// Figure-local pixels to surface pixels.
    #[must_use]
    pub const fn scale(&self) -> f64 {
        self.scale
    }

    /// The surface point its ground contact sits on.
    #[must_use]
    pub const fn at(&self) -> (f64, f64) {
        self.at
    }

    /// The root transform the whole figure hangs in.
    #[must_use]
    pub const fn root(&self) -> Resolved {
        self.root
    }
}

/// One placed shape and the keys it sorts on.
#[derive(Copy, Clone, Debug)]
struct Entry {
    depth: f64,
    order: u16,
    placed: Placed,
}

impl Entry {
    /// The value an unused slot holds.
    const BLANK: Self = Self {
        depth: 0.0,
        order: 0,
        placed: Placed {
            x: 0.0,
            y: 0.0,
            turn: 0.0,
            shape: Shape::Splat { radius: 0.0 },
            color: Color::rgb(0, 0, 0),
            seed: 0,
        },
    };
}

/// The buffers a figure is placed and sorted through.
///
/// Held by the caller across frames: a figure amounts to at most
/// [`MAX_PLACED`] shapes over [`MAX_JOINTS`] frames, both bounded, so drawing
/// one costs no allocation however many figures a scene holds — and the slots
/// are written rather than grown, so there is no capacity to run out of once
/// the figure is known to fit.
#[derive(Clone, Debug)]
pub struct Placement {
    entries: [Entry; MAX_PLACED],
    len: usize,
    frames: Frames,
}

impl Placement {
    /// Empty buffers.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: [Entry::BLANK; MAX_PLACED],
            len: 0,
            frames: Frames::new(),
        }
    }

    /// How many shapes the figure amounts to.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing has been placed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The placed shapes, far-first.
    #[must_use]
    pub fn parts(&self) -> impl ExactSizeIterator<Item = Placed> + '_ {
        self.entries[..self.len].iter().map(|entry| entry.placed)
    }

    /// Project one shape carried by `frame` into the next slot.
    ///
    /// The caller has already checked the whole figure fits, so the slot
    /// exists.
    fn push(&mut self, stance: &Stance, frame: Resolved, offset: Body, shape: Shape, color: Color) {
        let placed_at = frame.at.plus(frame.basis.apply(offset));
        let projected = project(stance.facing, placed_at);
        // The order is both the tie-break and the shape's identity, so a
        // splat's ripple is the same every frame.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "MAX_PLACED is asserted to fit u16 and bounds this index"
        )]
        let order = self.len as u16;
        self.entries[self.len] = Entry {
            depth: projected.depth,
            order,
            placed: Placed {
                x: stance.at.0 + projected.dx * stance.scale,
                y: stance.at.1 + projected.dy * stance.scale,
                turn: screen_turn(stance.facing, frame.basis),
                shape: shape.scaled(stance.scale),
                color,
                seed: order,
            },
        };
        self.len += 1;
    }

    /// Order the entries far-first.
    fn sort(&mut self) {
        // `total_cmp` orders every float, so no comparison is indeterminate,
        // and the order breaks a tie so two shapes at one depth paint as they
        // were authored. The order is unique, so this total comparator leaves
        // an unstable sort with nothing to be unstable about.
        self.entries[..self.len].sort_unstable_by(|a, b| {
            a.depth
                .total_cmp(&b.depth)
                .then_with(|| a.order.cmp(&b.order))
        });
    }
}

impl Default for Placement {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "rig/tests.rs"]
mod tests;
