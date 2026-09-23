//! The reference grid: which figures, in which poses, facing which way.
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
//!
//! # Which figures
//!
//! Each species' reference figure, drawn in every motion; then a least and a
//! most of each species — every build setting at its low end, then at its
//! high end — drawn walking. Between them they wear every form of every
//! feature, and the palest and darkest palettes any species admits, so a
//! bound the art is held to is held across the whole of what a record can
//! ask for rather than only the figure somebody happened to author.

use tairix_inline::ArrayVec;
use tairix_raster::shape::Placed;
use tairix_raster::Color;
use tairix_wintersun_net::value::Facing;

use crate::breath::Breath;
use crate::clip::Clip;
use crate::error::FigureError;
use crate::frame::Basis;
use crate::humanoid;
use crate::identity::{
    Build, EarForm, EyeShape, FaceShape, Features, HairStyle, HornForm, Identity, IdentityError,
    Palette, Setting, Spec, TailForm,
};
use crate::mesh::{self, Hoop, MAX_RINGS};
use crate::motion::{Kind, Motion};
use crate::plant::{Legs, Planted};
use crate::rig::{Frames, Part, Placement, Resolved, Rig, Stance};
use crate::shadow::{Contact, Light};
use crate::species::Species;

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

/// One cell of a figure's grid: a motion, a phase of it, and a heading.
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

/// Which of the shipped motions a figure is drawn in.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Sampling {
    /// Every one: a species' reference figure.
    Every,
    /// The walk alone: a figure that is in the grid to put a form or an
    /// extreme build in front of the harness, where one gait crossing both
    /// stances and both swings shows it.
    Walk,
}

/// One figure of the grid.
#[derive(Copy, Clone, Debug)]
pub struct Figure {
    /// Its stable name, for a ledger row or a sheet's file name.
    pub name: &'static str,
    /// The record it is built from.
    pub spec: Spec,
    /// Which motions it is drawn in.
    pub sampling: Sampling,
}

impl Figure {
    /// The checked record.
    ///
    /// # Errors
    ///
    /// Whatever [`Identity::new`] refuses of the spec, which for the grid's
    /// own figures is nothing — the crate's tests hold every one to it.
    pub fn identity(&self) -> Result<Identity, IdentityError> {
        Identity::new(self.spec)
    }

    /// The motions it is drawn in.
    #[must_use]
    pub fn kinds(&self) -> &'static [Kind] {
        match self.sampling {
            Sampling::Every => &Kind::ALL,
            Sampling::Walk => &[Kind::Walk],
        }
    }

    /// Its cells, heading innermost and motion outermost.
    pub fn cells(&self) -> impl Iterator<Item = Cell> + '_ {
        self.kinds().iter().flat_map(|kind| {
            (0..PHASES).flat_map(move |step| {
                FACINGS.into_iter().map(move |facing| Cell {
                    kind: *kind,
                    step,
                    facing,
                })
            })
        })
    }
}

/// The grid's figures, in the order the digest folds them and the ledger
/// lists them: the five species' references, then each species' least and
/// most.
pub const FIGURES: [Figure; 15] = [
    reference("human", HUMAN),
    reference("elf", ELF),
    reference("dwarf", DWARF),
    reference("beastkin", BEASTKIN),
    reference("dragonkin", DRAGONKIN),
    walking(
        "human-least",
        Species::Human,
        Setting::LOW,
        Features {
            face: FaceShape::Round,
            eyes: EyeShape::Round,
            ears: EarForm::Round,
            horns: None,
            tail: None,
            hair: None,
            volume: Setting::LOW,
        },
        palette(0, 0, 6, 0, 14),
    ),
    walking(
        "human-most",
        Species::Human,
        Setting::HIGH,
        Features {
            face: FaceShape::Broad,
            eyes: EyeShape::Narrow,
            ears: EarForm::Round,
            horns: None,
            tail: None,
            hair: Some(HairStyle::Shaggy),
            volume: Setting::HIGH,
        },
        palette(11, 9, 0, 0, 8),
    ),
    walking(
        "elf-least",
        Species::Elf,
        Setting::LOW,
        Features {
            face: FaceShape::Heart,
            eyes: EyeShape::Narrow,
            ears: EarForm::Pointed,
            horns: None,
            tail: None,
            hair: Some(HairStyle::Cropped),
            volume: Setting::LOW,
        },
        palette(0, 12, 7, 0, 10),
    ),
    walking(
        "elf-most",
        Species::Elf,
        Setting::HIGH,
        Features {
            face: FaceShape::Oval,
            eyes: EyeShape::Round,
            ears: EarForm::Long,
            horns: None,
            tail: None,
            hair: Some(HairStyle::Topknot),
            volume: Setting::HIGH,
        },
        palette(11, 10, 10, 0, 12),
    ),
    walking(
        "dwarf-least",
        Species::Dwarf,
        Setting::LOW,
        Features {
            face: FaceShape::Oval,
            eyes: EyeShape::Almond,
            ears: EarForm::Round,
            horns: None,
            tail: None,
            hair: Some(HairStyle::Cropped),
            volume: Setting::LOW,
        },
        palette(0, 2, 2, 0, 4),
    ),
    walking(
        "dwarf-most",
        Species::Dwarf,
        Setting::HIGH,
        Features {
            face: FaceShape::Round,
            eyes: EyeShape::Narrow,
            ears: EarForm::Round,
            horns: None,
            tail: None,
            hair: Some(HairStyle::Shaggy),
            volume: Setting::HIGH,
        },
        palette(11, 13, 0, 0, 11),
    ),
    walking(
        "beastkin-least",
        Species::Beastkin,
        Setting::LOW,
        Features {
            face: FaceShape::Oval,
            eyes: EyeShape::Round,
            ears: EarForm::Lop,
            horns: Some(HornForm::Curled),
            tail: Some(TailForm::Slender),
            hair: None,
            volume: Setting::LOW,
        },
        palette(0, 0, 5, 6, 9),
    ),
    walking(
        "beastkin-most",
        Species::Beastkin,
        Setting::HIGH,
        Features {
            face: FaceShape::Long,
            eyes: EyeShape::Almond,
            ears: EarForm::Upright,
            horns: Some(HornForm::Nubs),
            tail: None,
            hair: Some(HairStyle::Topknot),
            volume: Setting::HIGH,
        },
        palette(9, 12, 8, 0, 3),
    ),
    walking(
        "dragonkin-least",
        Species::Dragonkin,
        Setting::LOW,
        Features {
            face: FaceShape::Heart,
            eyes: EyeShape::Round,
            ears: EarForm::Pointed,
            horns: Some(HornForm::Curled),
            tail: Some(TailForm::Scaled),
            hair: Some(HairStyle::Short),
            volume: Setting::LOW,
        },
        palette(0, 11, 9, 5, 13),
    ),
    walking(
        "dragonkin-most",
        Species::Dragonkin,
        Setting::HIGH,
        Features {
            face: FaceShape::Long,
            eyes: EyeShape::Almond,
            ears: EarForm::Finned,
            horns: Some(HornForm::Spire),
            tail: Some(TailForm::Scaled),
            hair: None,
            volume: Setting::LOW,
        },
        palette(8, 0, 10, 0, 15),
    ),
];

/// The record `species`' reference figure is built from.
#[must_use]
pub const fn spec(species: Species) -> Spec {
    match species {
        Species::Human => HUMAN,
        Species::Elf => ELF,
        Species::Dwarf => DWARF,
        Species::Beastkin => BEASTKIN,
        Species::Dragonkin => DRAGONKIN,
    }
}

/// `species`' reference figure, checked.
///
/// # Errors
///
/// As [`Figure::identity`].
pub fn identity(species: Species) -> Result<Identity, IdentityError> {
    Identity::new(spec(species))
}

/// Every setting at the middle of its species' range.
const MIDDLE: Build = uniform(Setting(128));

const fn uniform(setting: Setting) -> Build {
    Build {
        height: setting,
        girth: setting,
        taper: setting,
        limbs: setting,
        head: setting,
    }
}

const fn palette(skin: u8, hair: u8, eyes: u8, markings: u8, accent: u8) -> Palette {
    Palette {
        skin,
        hair,
        eyes,
        markings,
        accent,
    }
}

const fn reference(name: &'static str, spec: Spec) -> Figure {
    Figure {
        name,
        spec,
        sampling: Sampling::Every,
    }
}

const fn walking(
    name: &'static str,
    species: Species,
    setting: Setting,
    features: Features,
    palette: Palette,
) -> Figure {
    Figure {
        name,
        spec: Spec {
            species,
            build: uniform(setting),
            features,
            palette,
        },
        sampling: Sampling::Walk,
    }
}

const HUMAN: Spec = Spec {
    species: Species::Human,
    build: MIDDLE,
    features: Features {
        face: FaceShape::Oval,
        eyes: EyeShape::Almond,
        ears: EarForm::Round,
        horns: None,
        tail: None,
        hair: Some(HairStyle::Short),
        volume: Setting(128),
    },
    palette: palette(4, 7, 1, 0, 0),
};

const ELF: Spec = Spec {
    species: Species::Elf,
    build: MIDDLE,
    features: Features {
        face: FaceShape::Long,
        eyes: EyeShape::Almond,
        ears: EarForm::Long,
        horns: None,
        tail: None,
        hair: Some(HairStyle::Shaggy),
        volume: Setting(128),
    },
    palette: palette(1, 1, 4, 0, 2),
};

const DWARF: Spec = Spec {
    species: Species::Dwarf,
    build: MIDDLE,
    features: Features {
        face: FaceShape::Broad,
        eyes: EyeShape::Round,
        ears: EarForm::Round,
        horns: None,
        tail: None,
        hair: Some(HairStyle::Topknot),
        volume: Setting(128),
    },
    palette: palette(5, 4, 6, 0, 1),
};

const BEASTKIN: Spec = Spec {
    species: Species::Beastkin,
    build: MIDDLE,
    features: Features {
        face: FaceShape::Heart,
        eyes: EyeShape::Narrow,
        ears: EarForm::Upright,
        horns: None,
        tail: Some(TailForm::Brush),
        hair: Some(HairStyle::Short),
        volume: Setting(128),
    },
    palette: palette(3, 5, 4, 0, 6),
};

const DRAGONKIN: Spec = Spec {
    species: Species::Dragonkin,
    build: MIDDLE,
    features: Features {
        face: FaceShape::Round,
        eyes: EyeShape::Narrow,
        ears: EarForm::Finned,
        horns: Some(HornForm::Swept),
        tail: Some(TailForm::Scaled),
        hair: Some(HairStyle::Cropped),
        volume: Setting(128),
    },
    palette: palette(3, 9, 8, 0, 7),
};

/// One figure of the grid, built once and posed per cell.
#[derive(Clone, Debug)]
pub struct Reference {
    rig: Rig,
    legs: Legs,
    motions: [Motion; Kind::ALL.len()],
}

impl Reference {
    /// Assemble `identity`'s rig, its legs and the shipped motion set.
    ///
    /// # Errors
    ///
    /// Whatever the rig, its rigging, its legs or a motion refuse — none of
    /// which is reachable for a checked identity, which is what the crate's
    /// own tests say.
    pub fn new(identity: &Identity) -> Result<Self, FigureError> {
        let rig = humanoid::rig(identity)?;
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
    /// One part at a time: a whole figure's rings are over ten kibibytes,
    /// which is not a thing to put on a boot stack when the caller only ever
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
        let own = frames.get(part.joint()).ok_or(FigureError::NoSuchJoint)?;
        let end = match part.end() {
            Some(joint) => Some(frames.get(joint).ok_or(FigureError::NoSuchJoint)?),
            None => None,
        };
        let rest = part.end().map(|joint| {
            let held = self.rig.joints()[joint.index()];
            (held.at, Basis::of(held.orientation))
        });
        mesh::carry(
            part.rings(),
            part.stretch(),
            part.at(),
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
