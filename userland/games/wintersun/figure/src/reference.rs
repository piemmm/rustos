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
//!
//! Last come two figures of each species drawn by [`plausible::figure`]
//! from fixed seeds, walking too, so what a designer's "surprise me" hands a
//! player is held to every bound an authored figure is.
//!
//! # One stage
//!
//! Every figure stands on the same stage — level ground, the one sun the
//! game lights its ground by too, one breath, and each motion's framing in a
//! square cell, never tighter than locomotion's — and the designer's preview
//! stands on it as well, so the figure a player is shown while designing is
//! drawn exactly as the harness measures it.
//!
//! [`plausible::figure`]: crate::plausible::figure

use tairix_inline::ArrayVec;
use tairix_raster::shape::Placed;
use tairix_raster::Color;
use tairix_rng::NonCryptoRng;
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
use crate::motion::{self, Kind};
use crate::plant::{Legs, Planted};
use crate::plausible;
use crate::pose::Pose;
use crate::rig::{Frames, Part, Placement, Resolved, Rig, Stance};
use crate::shadow::{Contact, Light};
use crate::species::Species;
use crate::tint::Tints;

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

/// The way `WinterSun`'s low sun travels over the ground, east and south.
///
/// From the north-west, the side a relief map is lit from so that a hill
/// reads as raised rather than sunk. One definition for the stage and the
/// game's scene alike — the ground's relief shading takes its direction from
/// here — so a figure is measured under the light it is drawn under, and a
/// figure and the slope it stands on are never lit from two sides.
pub const SUN_TOWARD: (i32, i32) = (3, 2);

/// How far above the horizon the sun stands, in radians: low, so shadows
/// are long and a rounded surface shades across its whole turn.
pub const SUN_ELEVATION: f64 = 0.55;

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

/// The pixel sides a figure is drawn at, smallest first.
///
/// The smallest is the readability floor — the size a figure is smallest on
/// screen, which the art bounds are really about — and the largest is the
/// biggest a figure is drawn, so the one a cost budget is taken at.
pub const SIDES: [u32; 3] = [32, 64, 128];

/// How far above and below its ground point a standing, walking or running
/// figure is drawn, in units of its rest reach: its crown and what the depth
/// axis lifts behind it, and the near foot of a stride, which the depth axis
/// draws below the ground point the figure stands on.
///
/// One framing for the three, because they are one continuum a figure moves
/// through, and the tightest any motion is framed at: no motion's cells draw
/// a figure larger than these do.
const STANDING: (f64, f64) = (1.02, 0.20);

/// How far above and below its ground point a figure in `kind` is drawn, in
/// units of its rest reach.
///
/// Measured off the outline of every ring of every build corner of every
/// species, at the grid's phases, either extreme of the breath and every
/// sixteenth of a turn, and rounded up to a hundredth; never less than
/// locomotion's, so a motion that reaches less far is framed as locomotion
/// is. An arm raised in front of a figure facing away is drawn up the screen
/// past its crown, which is why a motion that lifts the arms needs more room
/// above than one that swings them.
#[must_use]
pub const fn allowance(kind: Kind) -> (f64, f64) {
    match kind {
        Kind::Idle | Kind::Walk | Kind::Run => STANDING,
        Kind::Die => (1.05, STANDING.1),
        Kind::Sit => (STANDING.0, 0.30),
        Kind::Dodge => (1.10, STANDING.1),
        Kind::Hit => (1.11, STANDING.1),
        Kind::Cast => (1.12, STANDING.1),
        Kind::MeleeLight => (1.14, STANDING.1),
        Kind::Channel => (1.18, STANDING.1),
        Kind::Swim => (1.19, STANDING.1),
        Kind::MeleeHeavy => (1.22, STANDING.1),
        Kind::Draw => (1.24, STANDING.1),
        Kind::Climb => (1.27, STANDING.1),
        Kind::Stagger => (1.29, STANDING.1),
        Kind::Loose => (1.30, STANDING.1),
        Kind::Fall => (1.36, STANDING.1),
    }
}

/// The share of a cell's side left clear at its top and at its bottom.
const MARGIN: f64 = 0.02;

/// How much of a cell's side a figure's reach is drawn into in `kind`, all
/// of its allowance fitting between the margins, and where its ground
/// contact sits down the cell.
fn framing(kind: Kind) -> (f64, f64) {
    let (above, below) = allowance(kind);
    let fit = (1.0 - 2.0 * MARGIN) / (above + below);
    (fit, MARGIN + above * fit)
}

/// Where a figure reaching `reach` is drawn in `kind`, in a square cell of
/// `side` pixels: the scale that fits the whole of what it draws, and the
/// surface point its ground contact sits on.
///
/// Taken from the figure's own reach rather than a height stated here, so a
/// taller or broader figure fits the same cell with no second number.
///
/// # Errors
///
/// [`FigureError::ScaleUnreal`] for a reach or a side no figure can be
/// framed by.
pub fn fit(kind: Kind, reach: f64, side: u32) -> Result<(f64, (f64, f64)), FigureError> {
    let extent = f64::from(side);
    let (share, ground) = framing(kind);
    let scale = extent * share / reach;
    if !scale.is_finite() || scale <= 0.0 {
        return Err(FigureError::ScaleUnreal);
    }
    Ok((scale, (extent * 0.5, extent * ground)))
}

/// The side of the square cell a standing figure reaching `reach` fills when
/// drawn at `scale` surface pixels a unit: the inverse of [`fit`], at the
/// tightest framing any motion is drawn at.
///
/// What says which of the grid's [`SIDES`] a figure drawn anywhere else is
/// being drawn at. The tightest framing is the conservative reading: every
/// motion proven readable at a side in its own framing is drawn there at no
/// larger a scale than this.
#[must_use]
pub fn side_at(reach: f64, scale: f64) -> f64 {
    scale * reach / framing(Kind::Idle).0
}

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

/// One figure of the grid, or a record measured to the grid's bounds.
#[derive(Copy, Clone, Debug)]
pub struct Figure<'a> {
    /// Its stable name, for a ledger row or a sheet's file name.
    pub name: &'a str,
    /// The record it is built from.
    pub spec: Spec,
    /// Which motions it is drawn in.
    pub sampling: Sampling,
}

impl Figure<'_> {
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
pub const FIGURES: [Figure<'static>; 15] = [
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

/// A figure of the grid drawn by the plausible generator rather than
/// authored: its species and the seed its draw starts from.
#[derive(Copy, Clone, Debug)]
pub struct Sample {
    /// Its stable name, for a ledger row or a sheet's file name.
    pub name: &'static str,
    /// The species it is drawn as.
    pub species: Species,
    /// The seed its generator starts from.
    pub seed: u64,
}

impl Sample {
    /// The figure its seed draws, walking.
    ///
    /// # Errors
    ///
    /// Whatever [`plausible::figure`] refuses, which is nothing: the crate's
    /// tests hold every draw to a record.
    pub fn figure(&self) -> Result<Figure<'static>, IdentityError> {
        let drawn = plausible::figure(self.species, &mut NonCryptoRng::seed_from_u64(self.seed))?;
        Ok(Figure {
            name: self.name,
            spec: drawn.spec(),
            sampling: Sampling::Walk,
        })
    }
}

/// The grid's generated figures, two of each species.
pub const SAMPLES: [Sample; 10] = [
    sample("human-sample-1", Species::Human, 1),
    sample("human-sample-2", Species::Human, 2),
    sample("elf-sample-1", Species::Elf, 3),
    sample("elf-sample-2", Species::Elf, 4),
    sample("dwarf-sample-1", Species::Dwarf, 5),
    sample("dwarf-sample-2", Species::Dwarf, 6),
    sample("beastkin-sample-1", Species::Beastkin, 7),
    sample("beastkin-sample-2", Species::Beastkin, 8),
    sample("dragonkin-sample-1", Species::Dragonkin, 9),
    sample("dragonkin-sample-2", Species::Dragonkin, 10),
];

const fn sample(name: &'static str, species: Species, seed: u64) -> Sample {
    Sample {
        name,
        species,
        seed,
    }
}

/// Every figure of the grid, in the order the digest folds them and the
/// ledger lists them: the authored figures, then the generated ones.
pub fn grid() -> impl Iterator<Item = Result<Figure<'static>, IdentityError>> {
    let (authored, drawn): (&'static [Figure<'static>], &'static [Sample]) = (&FIGURES, &SAMPLES);
    authored
        .iter()
        .copied()
        .map(Ok)
        .chain(drawn.iter().map(Sample::figure))
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

const fn reference(name: &'static str, spec: Spec) -> Figure<'static> {
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
) -> Figure<'static> {
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

/// A figure built and stood on the stage: its rig, and the legs the planting
/// solve runs along.
///
/// The one path from a pose to a placement, which the grid and the
/// designer's preview both take.
#[derive(Clone, Debug)]
pub(crate) struct Staged {
    rig: Rig,
    legs: Legs,
}

impl Staged {
    /// Build `identity`'s rig and its legs.
    pub(crate) fn new(identity: &Identity) -> Result<Self, FigureError> {
        let rig = humanoid::rig(identity)?;
        let legs = Legs::new(&humanoid::rigging(&rig)?, humanoid::legs())?;
        Ok(Self { rig, legs })
    }

    pub(crate) const fn rig(&self) -> &Rig {
        &self.rig
    }

    pub(crate) const fn legs(&self) -> Legs {
        self.legs
    }

    pub(crate) fn retint(&mut self, tints: Tints) {
        self.rig.retint(tints);
    }

    /// `pose` with `breath` laid over it, solved onto the level ground with
    /// the root at `lift`.
    pub(crate) fn plant(
        &self,
        pose: &Pose,
        breath: Breath,
        lift: f64,
    ) -> Result<Planted, FigureError> {
        let rigging = humanoid::rigging(&self.rig)?;
        let posed = breath.overlay()?.applied(pose)?;
        let mut frames = Frames::new();
        rigging
            .posture(&posed)?
            .resolve(Resolved::REST, &mut frames);
        self.legs.plant(&rigging, &posed, &frames, LEVEL, lift)
    }

    /// Place `planted` facing `facing` under the stage's light, drawn at
    /// `scale` with its ground contact at the surface point `at`.
    pub(crate) fn place(
        &self,
        planted: &Planted,
        facing: Facing,
        scale: f64,
        at: (f64, f64),
        out: &mut Placement,
    ) -> Result<(), FigureError> {
        let stance = Stance::new(facing, scale, at, Reference::light()?)?.rooted(planted.root());
        humanoid::rigging(&self.rig)?
            .posture(&planted.pose())?
            .place(&stance, &[], out)
    }
}

/// One figure of the grid, built once and posed per cell.
#[derive(Clone, Debug)]
pub struct Reference {
    staged: Staged,
    motions: motion::Set,
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
        Ok(Self {
            staged: Staged::new(identity)?,
            motions: motion::Set::new()?,
        })
    }

    /// The rig it poses.
    #[must_use]
    pub const fn rig(&self) -> &Rig {
        self.staged.rig()
    }

    /// `kind`'s shipped clip.
    ///
    /// # Errors
    ///
    /// Whatever [`Clip::new`] refuses, which for the shipped tables is
    /// nothing.
    pub fn clip(&self, kind: Kind) -> Result<Clip<'_>, FigureError> {
        self.motions.clip(kind)
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
        self.staged.place(&planted, cell.facing, scale, at, out)?;
        Ok(planted)
    }

    /// The pose and root `cell` leaves the figure in, layers and planting
    /// solve included.
    ///
    /// # Errors
    ///
    /// Whatever the clip, the breath or the planting solve refuse.
    pub fn posed(&self, cell: Cell) -> Result<Planted, FigureError> {
        let clip = self.clip(cell.kind)?;
        let mut breath = Breath::new(BREATH.0, BREATH.1)?;
        breath.advance(cell.breathed())?;
        self.staged.plant(
            &clip.sample(cell.phase())?,
            breath,
            clip.root_at(cell.phase()),
        )
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
            let held = self.rig().joints()[joint.index()];
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

    /// The light the reference grid is lit by: the sun.
    ///
    /// # Errors
    ///
    /// [`FigureError::LightUnreal`] never, for the stated constants.
    pub fn light() -> Result<Light, FigureError> {
        Light::new(
            f64::from(SUN_TOWARD.0),
            f64::from(SUN_TOWARD.1),
            SUN_ELEVATION,
        )
    }

    /// The legs the planting solve runs along.
    #[must_use]
    pub const fn legs(&self) -> Legs {
        self.staged.legs()
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
