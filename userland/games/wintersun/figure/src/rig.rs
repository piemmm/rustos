//! The rig: a skeleton, the parts it carries, the sockets gear hangs on, and
//! the pass that turns a posture into painted strips.

use tairix_inline::ArrayVec;
use tairix_raster::surface::SUBPIXEL;
use tairix_raster::Color;
use tairix_util::mathf;
use tairix_wintersun_net::value::Facing;

use crate::error::FigureError;
use crate::frame::{project, toward_camera, Basis, Body, Heading, Rotation};
use crate::joint::{Joint, JointId, MAX_JOINTS};
use crate::mesh::{self, Ring, Stretch, BANDS, EDGES, MAX_RINGS, STRIP};
use crate::shadow::Light;
use crate::socket::{Mount, Socket};
use crate::tint::{Tint, Tints};

/// How many parts one rig is built from.
///
/// A bound on authored content, like the joint bound: the shipped rigs are
/// held to it at build time, and a figure that wants more parts is a
/// different figure rather than a bigger one. It is exactly the richest
/// figure the shipped humanoid builds, which that module asserts.
pub const MAX_PARTS: usize = 33;

/// How many equipment surfaces one figure carries at once.
pub const MAX_FITTED: usize = 8;

/// How many surfaces one placed figure amounts to.
pub const MAX_PLACED: usize = MAX_PARTS + MAX_FITTED;

const _: () = assert!(MAX_PLACED <= u16::MAX as usize);

/// One surface of the figure's own body, carried by the joint it rides.
///
/// Its fields are private and every builder checks what it is given, so a
/// part that exists is one that can be drawn; whether it fits a skeleton is
/// [`Rig::new`]'s question.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Part {
    joint: JointId,
    end: Option<JointId>,
    at: Body,
    rings: &'static [Ring],
    stretch: Stretch,
    tint: Tint,
    sort: Option<Body>,
}

impl Part {
    /// A part on `joint` at `at`, built from `rings` as authored and drawn in
    /// `tint`.
    ///
    /// The rings are borrowed rather than owned, because geometry is
    /// first-party code rather than input and a figure's whole buffer set has
    /// to fit the boot stack the cross-target verticals run on: rings held by
    /// value would more than double a `Rig`. A figure's build reaches them
    /// through a [`Stretch`] instead.
    ///
    /// # Errors
    ///
    /// As [`mesh::check`].
    pub fn new(
        joint: JointId,
        at: Body,
        rings: &'static [Ring],
        tint: Tint,
    ) -> Result<Self, FigureError> {
        mesh::check(rings)?;
        if !at.is_real() {
            return Err(FigureError::GeometryUnreal);
        }
        Ok(Self {
            joint,
            end: None,
            at,
            rings,
            stretch: Stretch::NONE,
            tint,
            sort: None,
        })
    }

    /// The same part with its far end carried by `end`.
    ///
    /// A thigh's lower rings ride the knee, so the surface bends through the
    /// joint instead of two rigid tubes meeting at an angle. `end` must be a
    /// direct child of the part's own joint, which is what lets a ring's
    /// position in the far joint's frame be derived rather than authored
    /// twice; [`Rig::new`] refuses anything else.
    #[must_use]
    pub fn spanning(mut self, end: JointId) -> Self {
        self.end = Some(end);
        self
    }

    /// The same part with its rings scaled by `stretch`.
    ///
    /// # Errors
    ///
    /// [`FigureError::GeometryUnreal`] for a stretch that is not real.
    pub fn stretched(mut self, stretch: Stretch) -> Result<Self, FigureError> {
        if !stretch.is_real() {
            return Err(FigureError::GeometryUnreal);
        }
        self.stretch = stretch;
        Ok(self)
    }

    /// The same part depth-sorted by `point`, stated where its ring centres
    /// are, rather than by the mean of its rings.
    ///
    /// For a surface layered over another at every heading — hair over a
    /// skull — the two share one point, so their depths are the same number
    /// and the authored order decides. Two means would tie only until the
    /// head tilted, and then break whichever way the tilt went. The point is
    /// carried by the part's own joint alone.
    ///
    /// # Errors
    ///
    /// [`FigureError::GeometryUnreal`] for a point that is not finite.
    pub fn sorted_at(mut self, point: Body) -> Result<Self, FigureError> {
        if !point.is_real() {
            return Err(FigureError::GeometryUnreal);
        }
        self.sort = Some(point);
        Ok(self)
    }

    /// The joint it rides.
    ///
    /// Naming the joint rather than a position is the whole of the
    /// joint-carries-mass rule: a shoulder cap and the arm that swings from
    /// it are the same joint's, so they cannot drift apart.
    #[must_use]
    pub const fn joint(&self) -> JointId {
        self.joint
    }

    /// The joint its far end is carried by, for a part that spans a bend.
    #[must_use]
    pub const fn end(&self) -> Option<JointId> {
        self.end
    }

    /// Where its own frame sits in its joint's.
    #[must_use]
    pub const fn at(&self) -> Body {
        self.at
    }

    /// The rings it is built from, as authored.
    #[must_use]
    pub const fn rings(&self) -> &'static [Ring] {
        self.rings
    }

    /// How the figure's build scales those rings.
    #[must_use]
    pub const fn stretch(&self) -> Stretch {
        self.stretch
    }

    /// The role its colour plays.
    #[must_use]
    pub const fn tint(&self) -> Tint {
        self.tint
    }

    /// How far it reaches from the joint it rides.
    #[must_use]
    pub fn reach(&self) -> f64 {
        let mut furthest = 0.0;
        for ring in self.rings {
            let ring = self.stretch.apply(*ring);
            furthest = mathf::fmax(furthest, self.at.plus(ring.at).length() + girth(ring));
        }
        furthest
    }
}

/// How far a ring bulges from its own centre.
fn girth(ring: Ring) -> f64 {
    mathf::fmax(ring.wide, ring.deep)
}

/// One surface of a piece of equipment, hung on a socket.
///
/// Stated against a socket rather than a joint, so one helm fits every rig
/// that offers a head and no gear knows a skeleton.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Fitted {
    /// The socket it hangs on.
    pub socket: Socket,
    /// Where it sits in that socket's frame.
    pub at: Body,
    /// The cross-sections it is built from.
    pub rings: &'static [Ring],
    /// Its tone before the light reaches it.
    pub color: Color,
    /// How far it leans from its socket's rest.
    pub turn: Rotation,
}

impl Fitted {
    /// A fitted surface on `socket` at `at`, resting as the socket does.
    ///
    /// # Errors
    ///
    /// As [`mesh::check`].
    pub fn new(
        socket: Socket,
        at: Body,
        rings: &'static [Ring],
        color: Color,
    ) -> Result<Self, FigureError> {
        mesh::check(rings)?;
        Ok(Self {
            socket,
            at,
            rings,
            color,
            turn: Rotation::REST,
        })
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

/// A skeleton, its parts, the sockets it offers, and the colours it is
/// drawn in.
#[derive(Clone, Debug)]
pub struct Rig {
    joints: ArrayVec<Joint, MAX_JOINTS>,
    parts: ArrayVec<Part, MAX_PARTS>,
    mounts: [Option<Mount>; Socket::COUNT],
    tints: Tints,
    reach: f64,
}

impl Rig {
    /// Assemble and check a rig drawn in `tints`.
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
    /// a dimension that is not finite or a mount scale that is not positive;
    /// and the two that carry the visual rule —
    /// [`FigureError::BearingJointWithoutMass`] for a joint that bears a
    /// child but draws nothing itself, and
    /// [`FigureError::JointBeyondParentReach`] for a child whose origin lies
    /// outside everything its parent draws.
    pub fn new(
        joints: &[Joint],
        parts: &[Part],
        mounts: &[(Socket, Mount)],
        tints: Tints,
    ) -> Result<Self, FigureError> {
        let mut rig = Self {
            joints: ArrayVec::new(),
            parts: ArrayVec::new(),
            mounts: [None; Socket::COUNT],
            tints,
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
            // A ring's position in the far joint's frame is derived from the
            // rest transform between the two, which only exists where the
            // far joint hangs directly from the near one.
            if let Some(end) = part.end {
                let child = rig
                    .joints
                    .get(end.index())
                    .ok_or(FigureError::NoSuchJoint)?;
                if child.parent != Some(part.joint) {
                    return Err(FigureError::LegNotAChain);
                }
            }
            rig.parts
                .try_push(*part)
                .map_err(|_| FigureError::TooManyParts)?;
        }

        for (socket, mount) in mounts {
            if mount.joint.index() >= rig.joints.len() {
                return Err(FigureError::NoSuchJoint);
            }
            if !mount.at.is_real()
                || !mount.orientation.is_real()
                || !mount.scale.is_finite()
                || mount.scale <= 0.0
            {
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

    /// The colours it is drawn in.
    #[must_use]
    pub const fn tints(&self) -> Tints {
        self.tints
    }

    /// Draw it in `tints` from now on.
    ///
    /// Nothing else about the rig depends on its colours, so a palette edit
    /// costs this and not a rebuild.
    pub fn retint(&mut self, tints: Tints) {
        self.tints = tints;
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
    /// The second test is one-sided on purpose: a part's reach is an outer
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
                    let bound = part.reach();
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
            for ring in part.rings {
                let ring = part.stretch.apply(*ring);
                let at = frame.at.plus(frame.basis.apply(part.at.plus(ring.at)));
                furthest = mathf::fmax(furthest, at.length() + girth(ring));
            }
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
        let seen = Seen {
            view: toward_camera(stance.heading),
            light: stance.light.toward(stance.heading),
        };

        for part in &self.rig.parts {
            let own = out.frames.frames[part.joint.index()];
            let end = part.end.map(|joint| out.frames.frames[joint.index()]);
            let rest = part.end.map(|joint| {
                let held = self.rig.joints[joint.index()];
                (held.at, Basis::of(held.orientation))
            });
            let surface = Surface {
                rings: part.rings,
                stretch: part.stretch,
                at: part.at,
                sort: part.sort,
                color: self.rig.tints.get(part.tint),
            };
            out.push(stance, seen, surface, own, end, rest)?;
        }
        for piece in fitted {
            let mount = self
                .rig
                .mount(piece.socket)
                .ok_or(FigureError::NoSuchSocket)?;
            if !piece.at.is_real()
                || !piece.turn.is_real()
                || piece.rings.iter().any(|ring| !ring.is_real())
            {
                return Err(FigureError::GeometryUnreal);
            }
            let carried = out.frames.frames[mount.joint.index()];
            let frame = Resolved {
                at: carried.at.plus(carried.basis.apply(mount.at)),
                basis: carried
                    .basis
                    .compose(Basis::of(mount.orientation).compose(Basis::of(piece.turn))),
            };
            // Gear is authored for the reference figure, so the body it sits
            // on scales it — where it sits in the socket as well as its size.
            let surface = Surface {
                rings: piece.rings,
                stretch: Stretch::uniform(mount.scale),
                at: piece.at.scaled(mount.scale),
                sort: None,
                color: piece.color,
            };
            out.push(stance, seen, surface, frame, None, None)?;
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
    heading: Heading,
    scale: f64,
    at: (f64, f64),
    root: Resolved,
    light: Light,
}

/// How a figure is seen: the direction toward the camera and the direction
/// the light travels, both in the figure's own frame.
#[derive(Copy, Clone, Debug, PartialEq)]
struct Seen {
    view: Body,
    light: Body,
}

/// One surface about to be placed: its authored rings, how the figure's
/// build scales them, where its frame sits on the joint that carries it, the
/// point it sorts by if not its rings' mean, and its colour.
#[derive(Copy, Clone, Debug)]
struct Surface {
    rings: &'static [Ring],
    stretch: Stretch,
    at: Body,
    sort: Option<Body>,
    color: Color,
}

impl Stance {
    /// A figure facing `facing`, drawn at `scale`, standing at `at`, under
    /// `light`.
    ///
    /// `scale` converts the figure-local pixels the rig is authored in to
    /// surface pixels — the figure's drawn height over [`Rig::reach`]'s own
    /// units — and `at` is the surface column and row its feet rest on. The
    /// light is the scene's, and is here because a surface cannot be shaded
    /// without it and a figure turning under a fixed light is what makes it
    /// read as solid. The root starts at rest; [`Self::rooted`] displaces
    /// it.
    ///
    /// # Errors
    ///
    /// [`FigureError::ScaleUnreal`] for a scale that is not finite and
    /// positive, and [`FigureError::GeometryUnreal`] for a ground point that
    /// is not finite.
    pub fn new(
        facing: Facing,
        scale: f64,
        at: (f64, f64),
        light: Light,
    ) -> Result<Self, FigureError> {
        if !scale.is_finite() || scale <= 0.0 {
            return Err(FigureError::ScaleUnreal);
        }
        if !at.0.is_finite() || !at.1.is_finite() {
            return Err(FigureError::GeometryUnreal);
        }
        Ok(Self {
            heading: Heading::of(facing),
            scale,
            at,
            root: Resolved::REST,
            light,
        })
    }

    /// The same stance with the whole figure displaced and tilted by `root`.
    #[must_use]
    pub const fn rooted(mut self, root: Resolved) -> Self {
        self.root = root;
        self
    }
}

/// A surface coordinate in the scan converter's own sub-pixel units.
///
/// Saturating rather than wrapping: a figure placed far off the surface
/// must stay off it, and a wrapped coordinate would fold it back across the
/// canvas.
fn subpixel(point: (f64, f64)) -> (i32, i32) {
    let snap = |value: f64| {
        let scaled = value * f64::from(SUBPIXEL);
        if scaled <= f64::from(i32::MIN) {
            i32::MIN
        } else if scaled >= f64::from(i32::MAX) {
            i32::MAX
        } else {
            mathf::round_i32(scaled)
        }
    };
    (snap(point.0), snap(point.1))
}

/// One placed surface and the keys it sorts on.
#[derive(Copy, Clone, Debug)]
struct Entry {
    depth: f64,
    order: u16,
    rings: u8,
    /// Where each shaded strip's boundaries cross each ring, strip-major —
    /// so one strip's two sides are two contiguous runs and a caller needs
    /// no gather to fill it.
    ///
    /// In the scan converter's own sub-pixel units, which is both what the
    /// painter hands it and half the memory a pair of reals would take. A
    /// figure's buffer has to sit on a boot stack, and the finer placement
    /// a real would carry is below what any fill can distinguish.
    edge: [[(i32, i32); MAX_RINGS]; EDGES],
    /// What each strip is filled with, light already applied.
    tone: [Color; BANDS],
}

impl Entry {
    /// The value an unused slot holds.
    const BLANK: Self = Self {
        depth: 0.0,
        order: 0,
        rings: 0,
        edge: [[(0, 0); MAX_RINGS]; EDGES],
        tone: [Color::rgb(0, 0, 0); BANDS],
    };
}

/// One shaded strip down a placed surface, ready to fill.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Strip<'a> {
    /// Which surface it belongs to, by the order the rig authored them.
    ///
    /// The identity a caller sorts, names or folds a surface by, which the
    /// depth order deliberately does not preserve.
    pub surface: u16,
    /// Its tone, with the light already in it.
    pub color: Color,
    /// The points down one boundary, in the scan converter's sub-pixel
    /// units.
    pub near: &'a [(i32, i32)],
    /// The points down the other, which the fill walks back along.
    pub far: &'a [(i32, i32)],
}

/// The buffers a figure is placed and sorted through.
///
/// Held by the caller across frames: a figure amounts to at most
/// [`MAX_PLACED`] surfaces over [`MAX_JOINTS`] frames, both bounded, so
/// drawing one costs no allocation however many figures a scene holds — and
/// the slots are written rather than grown, so there is no capacity to run
/// out of once the figure is known to fit.
#[derive(Clone, Debug)]
pub struct Placement {
    entries: [Entry; MAX_PLACED],
    len: usize,
    frames: Frames,
}

impl Placement {
    /// Empty buffers.
    #[must_use]
    #[allow(
        clippy::large_stack_arrays,
        reason = "the buffer is the caller's, held across frames, and this \
                  crate links no allocator to put it anywhere else. At about \
                  fourteen kibibytes it sits inside the boot stack of every \
                  target this runs on, and the alternative the lint suggests \
                  is the heap the figure path exists to avoid"
    )]
    pub const fn new() -> Self {
        Self {
            entries: [Entry::BLANK; MAX_PLACED],
            len: 0,
            frames: Frames::new(),
        }
    }

    /// How many surfaces the figure amounts to.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing has been placed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Every shaded strip of every surface, far-first.
    ///
    /// The order is the contract: painting them as they come composites the
    /// figure correctly, and a strip's own two boundaries never cross, so
    /// nothing inside one surface needs sorting.
    pub fn strips(&self) -> impl Iterator<Item = Strip<'_>> + '_ {
        self.entries[..self.len].iter().flat_map(|entry| {
            let rings = usize::from(entry.rings);
            (0..BANDS).map(move |band| Strip {
                surface: entry.order,
                color: entry.tone[band],
                near: &entry.edge[band][..rings],
                far: &entry.edge[band + 1][..rings],
            })
        })
    }

    /// The box every strip lies inside, in the scan converter's sub-pixel
    /// units, as `(left, top, right, bottom)` inclusive — or `None` when
    /// nothing is placed.
    ///
    /// What a scene sorts a figure into the pieces of a frame by, so a piece
    /// draws only the figures that can reach it.
    #[must_use]
    pub fn extent(&self) -> Option<(i32, i32, i32, i32)> {
        let mut bounds: Option<(i32, i32, i32, i32)> = None;
        for strip in self.strips() {
            for &(x, y) in strip.near.iter().chain(strip.far) {
                bounds = Some(match bounds {
                    None => (x, y, x, y),
                    Some((left, top, right, bottom)) => {
                        (left.min(x), top.min(y), right.max(x), bottom.max(y))
                    }
                });
            }
        }
        bounds
    }

    /// Carry one surface's rings through to the screen and store its strips.
    ///
    /// The caller has already checked the whole figure fits, so the slot
    /// exists.
    fn push(
        &mut self,
        stance: &Stance,
        seen: Seen,
        surface: Surface,
        own: Resolved,
        end: Option<Resolved>,
        rest: Option<(Body, Basis)>,
    ) -> Result<(), FigureError> {
        let hoops = mesh::carry(
            surface.rings,
            surface.stretch,
            surface.at,
            (own.at, own.basis),
            end.map(|frame| (frame.at, frame.basis)),
            rest,
        )?;
        let Some(middle) = hoops.get(hoops.len() / 2) else {
            return Ok(());
        };

        let slot = &mut self.entries[self.len];
        slot.rings = u8::try_from(hoops.len()).map_err(|_| FigureError::TooManyParts)?;
        let mut depth = 0.0;
        for (index, hoop) in hoops.iter().enumerate() {
            let start = mesh::near(*hoop, seen.view);
            depth += project(stance.heading, hoop.at).depth;
            for (edge, row) in slot.edge.iter_mut().enumerate() {
                // Bounded by `EDGES`, far below the mantissa's own range.
                #[allow(clippy::cast_precision_loss, reason = "bounded by EDGES")]
                let step = edge as f64;
                let (point, _) = hoop.surface(start + STRIP * step);
                let placed = project(stance.heading, point);
                row[index] = subpixel((
                    stance.at.0 + placed.dx * stance.scale,
                    stance.at.1 + placed.dy * stance.scale,
                ));
            }
        }

        // The tone is taken once per strip, at the middle ring, because a
        // strip is filled flat: reading it per ring would cost more and
        // change nothing that reaches the surface.
        let start = mesh::near(*middle, seen.view);
        for (band, tone) in slot.tone.iter_mut().enumerate() {
            // Bounded by `BANDS`, far below the mantissa's own range.
            #[allow(clippy::cast_precision_loss, reason = "bounded by BANDS")]
            let step = band as f64 + 0.5;
            let (_, normal) = middle.surface(start + STRIP * step);
            *tone = mesh::shaded(surface.color, mesh::level(normal, seen.light));
        }

        slot.depth = if let Some(point) = surface.sort {
            let local = surface.at.plus(surface.stretch.point(point));
            project(stance.heading, own.at.plus(own.basis.apply(local))).depth
        } else {
            // Bounded by `MAX_RINGS`, and a part with no rings returned above.
            #[allow(clippy::cast_precision_loss, reason = "bounded by MAX_RINGS")]
            let count = hoops.len() as f64;
            depth / count
        };
        // The order is the tie-break that keeps a caller's paint order
        // identical from frame to frame.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "MAX_PLACED is asserted to fit u16 and bounds this index"
        )]
        let order = self.len as u16;
        slot.order = order;
        self.len += 1;
        Ok(())
    }

    /// Order the entries far-first.
    fn sort(&mut self) {
        // `total_cmp` orders every float, so no comparison is indeterminate,
        // and the order breaks a tie so two surfaces at one depth paint as
        // they were authored. The order is unique, so this total comparator
        // leaves an unstable sort with nothing to be unstable about.
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
