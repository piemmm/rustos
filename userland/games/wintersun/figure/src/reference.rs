//! The reference grid: which figure, in which poses, facing which way.
//!
//! One definition with two consumers. The cross-target digest folds it, and
//! the art harness (`cargo xtask artsheet`) renders it into the contact
//! sheets and measures them. Two grids that agreed today would be two grids
//! to keep in step, and the number a vertical asserts would stop describing
//! the picture a reviewer looked at.
//!
//! The pose depends on the motion and the phase and on nothing else — in
//! particular not on the heading, so the four headings of a cell are one
//! figure seen four ways rather than four figures.

use tairix_inline::ArrayVec;
use tairix_raster::shape::Placed;
use tairix_raster::Color;
use tairix_wintersun_net::value::Facing;

use crate::breath::Breath;
use crate::clip::Clip;
use crate::error::FigureError;
use crate::frame::Basis;
use crate::humanoid;
use crate::mesh::{self, Hoop, MAX_RINGS};
use crate::motion::{Kind, Motion};
use crate::plant::{Legs, Planted};
use crate::rig::{Frames, Part, Placement, Resolved, Rig, Stance};
use crate::shadow::{Contact, Light};

/// How many phases of each motion the grid covers.
///
/// Enough to see a cycle rather than a pose: eight samples cross both
/// stances and both swings. They land on the *boundaries* of the run's two
/// flight windows rather than inside them, so what a clip does while neither
/// foot is down is folded by the digest on its own finer grid rather than
/// drawn here.
pub const PHASES: usize = 8;

/// The headings the grid covers.
///
/// Four quarters. The draw order reverses across the turnaround — a face
/// sorts behind the skull facing away and in front of it facing the camera —
/// so a grid that never crossed it would miss the property the one body
/// frame exists for.
pub const FACINGS: [Facing; 4] = [
    Facing(0x0000),
    Facing(0x4000),
    Facing(0x8000),
    Facing(0xC000),
];

/// The ground the reference figures stand on.
///
/// Level, so the planting solve has to hand back the articulation the clip
/// authored: a sheet is a drawing of the clip, and a slope would be a
/// drawing of the solve. What the solve does off the level is probed
/// separately by the digest.
pub const LEVEL: [f64; 2] = [0.0, 0.0];

/// Where the light comes from: across the ground, into the scene, and how
/// far above the horizon, in radians.
pub const LIGHT: (f64, f64, f64) = (-0.6, 0.8, 0.55);

/// The contact shadow's footprint radius in figure-local units, and its tone
/// at full contact.
///
/// A darkening where the figure meets the ground, not a silhouette painted
/// on it: at a little over two thirds transparent and a footprint the width
/// of the stance, it says where the figure is standing without reading as a
/// second object lying beside it.
pub const SHADOW: (f64, Color) = (7.5, Color::rgba(0x0C, 0x0E, 0x12, 0x62));

/// The breath laid over every pose: its period in seconds, its depth as a
/// fraction of the spine's travel, and how far the grid advances it per
/// pose.
///
/// An always-on layer, so the overlay summing and its clamp back into range
/// are part of every cell rather than something a caller must remember.
pub const BREATH: (f64, f64, f64) = (4.5, 0.06, 0.37);

/// One cell of the grid: a motion, a phase of it, and a heading.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Cell {
    /// Which shipped motion.
    pub kind: Kind,
    /// Which of [`PHASES`] samples of its cycle.
    pub step: usize,
    /// Which way the figure faces.
    pub facing: Facing,
}

impl Cell {
    /// How many cells the grid holds.
    pub const COUNT: usize = Kind::ALL.len() * PHASES * FACINGS.len();

    /// The cell at `index`, walking heading innermost and motion outermost.
    #[must_use]
    pub fn at(index: usize) -> Option<Self> {
        if index >= Self::COUNT {
            return None;
        }
        let facing = FACINGS[index % FACINGS.len()];
        let pose = index / FACINGS.len();
        Some(Self {
            kind: Kind::ALL[pose / PHASES],
            step: pose % PHASES,
            facing,
        })
    }

    /// Where in its motion's cycle it sits.
    #[must_use]
    pub fn phase(self) -> f64 {
        real(self.step) / real(PHASES)
    }

    /// How far the breath has run by this pose.
    ///
    /// A function of the motion and the phase and not of the heading, so one
    /// pose is drawn four ways rather than four poses drawn once each.
    fn breathed(self) -> f64 {
        let pose = self
            .kind
            .index()
            .saturating_mul(PHASES)
            .saturating_add(self.step);
        real(pose) * BREATH.2
    }
}

/// The shipped figure, built once and posed per cell.
#[derive(Clone, Debug)]
pub struct Reference {
    rig: Rig,
    legs: Legs,
    motions: [Motion; Kind::ALL.len()],
}

impl Reference {
    /// Assemble the shipped rig, its legs and its motion set.
    ///
    /// # Errors
    ///
    /// Whatever the rig, its rigging, its legs or a motion refuse — none of
    /// which is reachable for the shipped set, which is what the crate's own
    /// tests say.
    pub fn new() -> Result<Self, FigureError> {
        let rig = humanoid::rig()?;
        let legs = Legs::new(&humanoid::rigging(&rig)?, humanoid::legs())?;
        Ok(Self {
            rig,
            legs,
            motions: [
                Motion::new(Kind::Idle)?,
                Motion::new(Kind::Walk)?,
                Motion::new(Kind::Run)?,
            ],
        })
    }

    /// The rig it poses.
    #[must_use]
    pub const fn rig(&self) -> &Rig {
        &self.rig
    }

    /// `kind`'s shipped clip.
    ///
    /// # Errors
    ///
    /// Whatever [`Clip::new`] refuses, which for the shipped tables is
    /// nothing.
    pub fn clip(&self, kind: Kind) -> Result<Clip<'_>, FigureError> {
        self.motions[kind.index()].clip()
    }

    /// Pose the figure for `cell` and place it, drawn at `scale` with its
    /// ground contact at the surface point `at`.
    ///
    /// Answers what the planting solve reported, so a caller can record the
    /// root it stood in and the ground it missed.
    ///
    /// # Errors
    ///
    /// Whatever the clip, the breath, the planting solve or the placement
    /// refuse; and [`FigureError::ScaleUnreal`] for a scale that is not
    /// finite and positive.
    pub fn place(
        &self,
        cell: Cell,
        scale: f64,
        at: (f64, f64),
        out: &mut Placement,
    ) -> Result<Planted, FigureError> {
        let planted = self.posed(cell)?;
        let stance = Stance::new(cell.facing, scale, at, Self::light()?)?.rooted(planted.root());
        humanoid::rigging(&self.rig)?
            .posture(&planted.pose())?
            .place(&stance, &[], out)?;
        Ok(planted)
    }

    /// The pose and root `cell` leaves the figure in, layers and planting
    /// solve included.
    ///
    /// # Errors
    ///
    /// Whatever the clip, the breath or the planting solve refuse.
    pub fn posed(&self, cell: Cell) -> Result<Planted, FigureError> {
        let rigging = humanoid::rigging(&self.rig)?;
        let clip = self.clip(cell.kind)?;
        let mut breath = Breath::new(BREATH.0, BREATH.1)?;
        breath.advance(cell.breathed())?;
        let posed = breath.overlay()?.applied(&clip.sample(cell.phase())?)?;

        let mut frames = Frames::new();
        rigging
            .posture(&posed)?
            .resolve(Resolved::REST, &mut frames);
        self.legs
            .plant(&rigging, &posed, &frames, LEVEL, clip.root_at(cell.phase()))
    }

    /// Where `part`'s rings end up for `cell`, given the resolve `frames`
    /// holds.
    ///
    /// One part at a time: a whole figure's rings are ten kibibytes, which
    /// is not a thing to put on a boot stack when the caller only ever
    /// looks at one surface before moving on.
    ///
    /// # Errors
    ///
    /// [`FigureError::NoSuchJoint`] if the resolve did not cover the part's
    /// joints, and whatever the carry refuses.
    pub fn surfaces(
        &self,
        part: &Part,
        frames: &Frames,
    ) -> Result<ArrayVec<Hoop, MAX_RINGS>, FigureError> {
        let own = frames.get(part.joint).ok_or(FigureError::NoSuchJoint)?;
        let end = match part.end {
            Some(joint) => Some(frames.get(joint).ok_or(FigureError::NoSuchJoint)?),
            None => None,
        };
        let rest = part.end.map(|joint| {
            let held = self.rig.joints()[joint.index()];
            (held.at, Basis::of(held.orientation))
        });
        mesh::carry(
            part.rings,
            part.at,
            (own.at, own.basis),
            end.map(|frame| (frame.at, frame.basis)),
            rest,
        )
    }

    /// The contact shadow under a figure standing `lift` above its ground
    /// point, drawn at `scale` around `at`.
    ///
    /// # Errors
    ///
    /// Whatever the light, the contact or the cast refuse.
    pub fn shadow(lift: f64, scale: f64, at: (f64, f64)) -> Result<Placed, FigureError> {
        Contact::new(SHADOW.0, SHADOW.1)?.cast(Self::light()?, lift, scale, at)
    }

    /// The light the reference grid is lit by.
    ///
    /// # Errors
    ///
    /// [`FigureError::LightUnreal`] never, for the stated constants.
    pub fn light() -> Result<Light, FigureError> {
        Light::new(LIGHT.0, LIGHT.1, LIGHT.2)
    }

    /// The legs the planting solve runs along.
    #[must_use]
    pub const fn legs(&self) -> Legs {
        self.legs
    }
}

/// A grid count as a real, exactly: every count here is a cell index or a
/// sample count, far below the mantissa's own range.
fn real(count: usize) -> f64 {
    #[allow(clippy::cast_precision_loss, reason = "bounded by the grid")]
    let value = count as f64;
    value
}

#[cfg(test)]
#[path = "reference/tests.rs"]
mod tests;
