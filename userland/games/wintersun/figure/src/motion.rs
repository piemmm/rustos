//! The shipped motion set: every clip a `WinterSun` figure is animated by.
//!
//! Seventeen, in three families. Locomotion — idle, walk, run — is paced by
//! the gait. Actions and the reactions to them — a dodge, the light and heavy
//! melee blows, the bow's draw and loose, a cast and its channel, a flinch
//! and a stagger — are windup, active and recovery, authored across all three
//! in phase and stretched onto time by whoever owns the action. States —
//! falling, dying, sitting, swimming, climbing — last as long as the state
//! does.
//!
//! # How the leg curves were arrived at
//!
//! Not by eye. A clip whose feet are on the floor states a **foot path** —
//! where each foot is against its hip, and how much of the leg's turn the
//! ankle levels it by — and wherever a foot is down the path is the floor,
//! wherever the clip holds the body. The hip, knee and ankle keys are that
//! path put through the planting layer's own two-bone solve and rounded to
//! six places, and this module's tests solve every key again from its path.
//!
//! The pelvis sits at a fixed height, so a clip states how deep into its own
//! legs the body stands, phase by phase, and keys its root height from that
//! depth where its legs are keyed. Between two keys the body and a planted
//! foot are then interpolated along one line, and `quality::grounding`
//! measures the two halves against each other.
//!
//! A falling, swimming or climbing figure has no floor under its feet, and
//! its legs are authored directly; `quality::penetration` holds them above
//! the floor instead.
//!
//! # One cycle, two sides
//!
//! A left and a right limb do the same thing half a turn apart in a gait, so
//! only one side of a locomotion cycle is authored and the other is that cycle
//! rotated half a turn. Two tables that must stay each other's mirror image
//! are two things to keep in step.

use tairix_inline::ArrayVec;

use crate::clip::{Clip, Curve, Event, Key, Lift, Loop, Segments, Timing, Travel};
use crate::error::FigureError;
use crate::humanoid::{SHANK_LENGTH, THIGH_LENGTH};
use crate::pose::Param;

mod action;
mod locomotion;
mod state;

use locomotion::{RUN_HALF_STEP, RUN_STANCE, WALK_HALF_STEP, WALK_STANCE};

/// How many curves one motion drives.
///
/// A clip carries at most one curve per parameter — [`Clip::new`] refuses a
/// second — so the parameter count is the bound rather than a number chosen
/// beside it.
pub const MAX_CURVES: usize = Param::COUNT;

/// Which shipped motion.
///
/// Declared in the order [`Self::ALL`] lists them, which is the order every
/// table of motions is held in.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Kind {
    /// Standing: a slow weight shift and a glance, so a waiting figure does
    /// not read as a paused game.
    Idle,
    /// Walking at the stride its foot path implies.
    Walk,
    /// Running: a longer step, a deeper crouch, a higher knee, and a cycle
    /// short enough that both feet leave the ground.
    Run,
    /// A crouch, a dash with both feet off the ground, and a landing.
    Dodge,
    /// A one-handed horizontal slash.
    MeleeLight,
    /// A two-handed overhead chop.
    MeleeHeavy,
    /// Raising the bow and drawing it to the face.
    Draw,
    /// Releasing the string from full draw.
    Loose,
    /// Gathering power and thrusting it forward.
    Cast,
    /// Holding a sustained spell out in front.
    Channel,
    /// A flinch from a blow.
    Hit,
    /// Knocked back a step.
    Stagger,
    /// Nothing underfoot.
    Fall,
    /// Collapsing into a squat and slumping over.
    Die,
    /// Seated low, knees up.
    Sit,
    /// Treading water.
    Swim,
    /// Hand over hand up a face.
    Climb,
}

impl Kind {
    /// Every shipped motion.
    pub const ALL: [Self; 17] = [
        Self::Idle,
        Self::Walk,
        Self::Run,
        Self::Dodge,
        Self::MeleeLight,
        Self::MeleeHeavy,
        Self::Draw,
        Self::Loose,
        Self::Cast,
        Self::Channel,
        Self::Hit,
        Self::Stagger,
        Self::Fall,
        Self::Die,
        Self::Sit,
        Self::Swim,
        Self::Climb,
    ];

    /// Its position in [`Self::ALL`].
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Its stable name, for a ledger row or a diagnostic.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Walk => "walk",
            Self::Run => "run",
            Self::Dodge => "dodge",
            Self::MeleeLight => "melee-light",
            Self::MeleeHeavy => "melee-heavy",
            Self::Draw => "draw",
            Self::Loose => "loose",
            Self::Cast => "cast",
            Self::Channel => "channel",
            Self::Hit => "hit",
            Self::Stagger => "stagger",
            Self::Fall => "fall",
            Self::Die => "die",
            Self::Sit => "sit",
            Self::Swim => "swim",
            Self::Climb => "climb",
        }
    }

    /// How far one cycle was authored to carry the figure, or `None` for a
    /// motion the gait does not pace.
    ///
    /// The authoring intent rather than a measurement: what the foot path
    /// was built to give. [`Gait::fitted`] measures the clip's *actual*
    /// stride, and a test holds the two together — which is what stops the
    /// path's documentation drifting from the keys it produced.
    ///
    /// [`Gait::fitted`]: crate::gait::Gait::fitted
    #[must_use]
    pub const fn stride(self) -> Option<f64> {
        match self {
            Self::Walk => Some(2.0 * WALK_HALF_STEP / WALK_STANCE),
            Self::Run => Some(2.0 * RUN_HALF_STEP / RUN_STANCE),
            _ => None,
        }
    }

    /// Which part of a performance it plays on.
    #[must_use]
    pub const fn layer(self) -> Layer {
        match self {
            Self::Idle | Self::Walk | Self::Run => Layer::Locomotion,
            Self::MeleeLight
            | Self::Draw
            | Self::Loose
            | Self::Cast
            | Self::Channel
            | Self::Hit => Layer::Upper,
            Self::Dodge
            | Self::MeleeHeavy
            | Self::Stagger
            | Self::Fall
            | Self::Die
            | Self::Sit
            | Self::Swim
            | Self::Climb => Layer::Body,
        }
    }

    /// What the figure's feet are on while it plays.
    #[must_use]
    pub const fn support(self) -> Support {
        match self {
            Self::Fall => Support::Air,
            Self::Swim => Support::Water,
            Self::Climb => Support::Wall,
            _ => Support::Ground,
        }
    }

    /// Everything its clip is assembled from.
    const fn authored(self) -> Authored {
        match self {
            Self::Idle => Authored::cycle(
                locomotion::IDLE_SECONDS,
                &locomotion::IDLE_CURVES,
                &NO_EVENTS,
            )
            .lifted(&locomotion::IDLE_LIFT),
            Self::Walk => Authored::cycle(
                locomotion::WALK_SECONDS,
                &locomotion::WALK_CURVES,
                &locomotion::FOOTSTEPS,
            )
            .lifted(&locomotion::WALK_LIFT),
            Self::Run => Authored::cycle(
                locomotion::RUN_SECONDS,
                &locomotion::RUN_CURVES,
                &locomotion::FOOTSTEPS,
            )
            .lifted(&locomotion::RUN_LIFT),
            Self::Dodge => {
                Authored::action(action::DODGE, &action::DODGE_CURVES, &action::DODGE_EVENTS)
                    .lifted(&action::DODGE_LIFT)
                    .travelling(&action::DODGE_TRAVEL)
            }
            Self::MeleeLight => Authored::action(
                action::MELEE_LIGHT,
                &action::MELEE_LIGHT_CURVES,
                &action::MELEE_LIGHT_EVENTS,
            ),
            Self::MeleeHeavy => Authored::action(
                action::MELEE_HEAVY,
                &action::MELEE_HEAVY_CURVES,
                &action::MELEE_HEAVY_EVENTS,
            )
            .lifted(&action::HEAVY_LIFT),
            Self::Draw => Authored::action(action::DRAW, &action::DRAW_CURVES, &NO_EVENTS),
            Self::Loose => {
                Authored::action(action::LOOSE, &action::LOOSE_CURVES, &action::LOOSE_EVENTS)
            }
            Self::Cast => {
                Authored::action(action::CAST, &action::CAST_CURVES, &action::CAST_EVENTS)
            }
            Self::Channel => {
                Authored::cycle(action::CHANNEL_SECONDS, &action::CHANNEL_CURVES, &NO_EVENTS)
            }
            Self::Hit => Authored::action(action::HIT, &action::HIT_CURVES, &NO_EVENTS),
            Self::Stagger => Authored::action(
                action::STAGGER,
                &action::STAGGER_CURVES,
                &action::STAGGER_EVENTS,
            )
            .lifted(&action::STAGGER_LIFT),
            Self::Fall => Authored::cycle(state::FALL_SECONDS, &state::FALL_CURVES, &NO_EVENTS),
            Self::Die => {
                Authored::once(state::DIE_SECONDS, &state::DIE_CURVES).lifted(&state::DIE_LIFT)
            }
            Self::Sit => Authored::cycle(state::SIT_SECONDS, &state::SIT_CURVES, &NO_EVENTS)
                .lifted(&state::SIT_LIFT),
            Self::Swim => Authored::cycle(state::SWIM_SECONDS, &state::SWIM_CURVES, &NO_EVENTS),
            Self::Climb => Authored::cycle(state::CLIMB_SECONDS, &state::CLIMB_CURVES, &NO_EVENTS),
        }
    }
}

/// Which part of a performance a clip plays on.
///
/// A performance is locomotion under everything, with an action over it: one
/// that takes the whole body, or one that takes only the trunk, head and
/// arms and leaves the legs to whatever the figure is doing.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Layer {
    /// Standing, walking and running, paced by the gait.
    Locomotion,
    /// The whole body, over locomotion.
    Body,
    /// The trunk, head and arms, over whatever the legs are doing.
    Upper,
}

/// What a figure's feet are on while a clip plays.
///
/// Decides what the clip is held to: a figure standing on the ground has to
/// land its feet on the floor, and one with nothing under it only has to keep
/// them out of it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Support {
    /// The floor.
    Ground,
    /// Nothing at all.
    Air,
    /// Deep water.
    Water,
    /// A face in front of the figure.
    Wall,
}

/// An action clip's authored segments and the reference durations it is
/// played at until the action that owns it states its own.
#[derive(Copy, Clone, Debug, PartialEq)]
struct Action {
    /// The phase the windup gives way to the active segment at.
    active: f64,
    /// The phase the active segment gives way to the recovery at.
    recovery: f64,
    /// How long windup, active and recovery last, in seconds.
    seconds: [f64; 3],
}

impl Action {
    const fn new(active: f64, recovery: f64, seconds: [f64; 3]) -> Self {
        Self {
            active,
            recovery,
            seconds,
        }
    }
}

/// Everything a shipped clip is assembled from.
#[derive(Copy, Clone, Debug)]
struct Authored {
    seconds: f64,
    repeat: Loop,
    curves: &'static [Keyed],
    events: &'static [Event],
    lift: Option<&'static [Key]>,
    travel: Option<&'static [Key]>,
    action: Option<Action>,
}

impl Authored {
    /// A clip that cycles.
    const fn cycle(seconds: f64, curves: &'static [Keyed], events: &'static [Event]) -> Self {
        Self {
            seconds,
            repeat: Loop::Wrap,
            curves,
            events,
            lift: None,
            travel: None,
            action: None,
        }
    }

    /// A clip that plays once and holds its last pose.
    const fn once(seconds: f64, curves: &'static [Keyed]) -> Self {
        Self {
            repeat: Loop::Hold,
            ..Self::cycle(seconds, curves, &NO_EVENTS)
        }
    }

    /// An action, played once at its reference timing.
    const fn action(action: Action, curves: &'static [Keyed], events: &'static [Event]) -> Self {
        let [windup, active, recovery] = action.seconds;
        Self {
            seconds: windup + active + recovery,
            repeat: Loop::Hold,
            curves,
            events,
            lift: None,
            travel: None,
            action: Some(action),
        }
    }

    const fn lifted(mut self, keys: &'static [Key]) -> Self {
        self.lift = Some(keys);
        self
    }

    const fn travelling(mut self, keys: &'static [Key]) -> Self {
        self.travel = Some(keys);
        self
    }
}

/// A shipped clip, holding the curve array a [`Clip`] borrows.
///
/// A clip borrows its curves and its curves borrow their keys, so a shipped
/// one cannot be a bare constant: this owns the curve array and hands out a
/// clip against it.
#[derive(Clone, Debug)]
pub struct Motion {
    kind: Kind,
    curves: ArrayVec<Curve<'static>, MAX_CURVES>,
    authored: Authored,
}

impl Motion {
    /// Assemble the shipped clip for `kind`.
    ///
    /// # Errors
    ///
    /// Whatever [`Curve::new`] refuses about the tables — only reachable if
    /// one is edited into something a parameter's range does not hold, which
    /// is the point of checking it here.
    pub fn new(kind: Kind) -> Result<Self, FigureError> {
        let authored = kind.authored();
        let mut curves = ArrayVec::new();
        for (param, keys) in authored.curves {
            curves
                .try_push(Curve::new(*param, keys)?)
                .map_err(|_| FigureError::DuplicateCurve)?;
        }
        Ok(Self {
            kind,
            curves,
            authored,
        })
    }

    /// Which motion it is.
    #[must_use]
    pub const fn kind(&self) -> Kind {
        self.kind
    }

    /// The clip, borrowing this motion's curves, played at its reference
    /// timing where it is an action.
    ///
    /// # Errors
    ///
    /// Whatever [`Clip::new`], [`Lift::new`], [`Travel::new`],
    /// [`Segments::new`], [`Timing::new`] or [`Clip::acting`] refuse, which
    /// for the shipped tables is nothing.
    pub fn clip(&self) -> Result<Clip<'_>, FigureError> {
        let authored = self.authored;
        let mut clip = Clip::new(
            authored.seconds,
            authored.repeat,
            &self.curves,
            authored.events,
        )?;
        if let Some(keys) = authored.lift {
            clip = clip.lifting(Lift::new(keys)?)?;
        }
        if let Some(keys) = authored.travel {
            clip = clip.travelling(Travel::new(keys)?);
        }
        if let Some(action) = authored.action {
            let [windup, active, recovery] = action.seconds;
            clip = clip.acting(
                Segments::new(action.active, action.recovery)?,
                Timing::new(windup, active, recovery)?,
            )?;
        }
        Ok(clip)
    }
}

/// Every shipped motion, held so the clips can borrow their curves.
#[derive(Clone, Debug)]
pub struct Set {
    motions: ArrayVec<Motion, { Kind::ALL.len() }>,
}

impl Set {
    /// Assemble every shipped motion.
    ///
    /// # Errors
    ///
    /// As [`Motion::new`].
    pub fn new() -> Result<Self, FigureError> {
        let mut motions = ArrayVec::new();
        for kind in Kind::ALL {
            motions
                .try_push(Motion::new(kind)?)
                .map_err(|_| FigureError::TooManyStates)?;
        }
        Ok(Self { motions })
    }

    /// `kind`'s clip.
    ///
    /// # Errors
    ///
    /// As [`Motion::clip`].
    pub(crate) fn clip(&self, kind: Kind) -> Result<Clip<'_>, FigureError> {
        self.motions
            .get(kind.index())
            .ok_or(FigureError::NoSuchClip)?
            .clip()
    }

    /// Every clip, where [`Kind::index`] puts it.
    ///
    /// # Errors
    ///
    /// As [`Motion::clip`].
    pub fn clips(&self) -> Result<Clips<'_>, FigureError> {
        let mut clips = ArrayVec::new();
        for motion in &self.motions {
            clips
                .try_push(motion.clip()?)
                .map_err(|_| FigureError::TooManyStates)?;
        }
        Ok(Clips(clips))
    }
}

/// Every shipped clip, indexed by [`Kind::index`].
///
/// Only [`Set::clips`] makes one, so a machine borrowing the table cannot be
/// handed the clips in an order that plays a walk when a run was asked for.
#[derive(Clone, Debug)]
pub struct Clips<'a>(ArrayVec<Clip<'a>, { Kind::ALL.len() }>);

impl<'a> Clips<'a> {
    /// The table, for a machine to borrow.
    pub(crate) fn table(&self) -> &[Clip<'a>] {
        &self.0
    }
}

/// One parameter's shipped curve.
type Keyed = (Param, &'static [Key]);

/// A clip with nothing to announce.
const NO_EVENTS: [Event; 0] = [];

/// The same cycle half a turn on, which is what the other side is doing.
///
/// The keys must be evenly spaced with the last repeating the first, which
/// is what makes a rotation by half the distinct keys a half-cycle shift;
/// a test holds every shipped table to it.
const fn opposite<const N: usize>(keys: &[Key; N]) -> [Key; N] {
    let mut out = *keys;
    let cycle = N - 1;
    let half = cycle / 2;
    let mut index = 0;
    while index < N {
        let from = keys[(index + half) % cycle];
        out[index] = Key {
            phase: keys[index].phase,
            value: from.value,
            easing: from.easing,
        };
        index += 1;
    }
    out
}

/// The leg a root height is measured against: straight, hip to ankle.
///
/// Taken from the same bone lengths the rig is built from, so a root height
/// is a fraction of the figure's own leg rather than of a number repeated
/// beside it.
const LEG_LENGTH: f64 = THIGH_LENGTH + SHANK_LENGTH;

/// `height` figure-local units as the fraction a root-height key carries.
const fn rooted(height: f64) -> f64 {
    height / LEG_LENGTH
}

/// A smoothstep across `0..=1`, clamped outside it: still at both ends.
///
/// A polynomial, so a depth profile built from it is exact at compile time.
const fn smooth(t: f64) -> f64 {
    let t = if t < 0.0 {
        0.0
    } else if t > 1.0 {
        1.0
    } else {
        t
    };
    t * t * (3.0 - 2.0 * t)
}

/// A body standing a stated depth into its own legs, phase by phase.
///
/// The profile a clip's root height is keyed from, and the one its planted
/// legs were solved at, so the two halves are one statement.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Sink {
    Dodge,
    Heavy,
    Stagger,
    Die,
}

impl Sink {
    /// How deep into its legs the body stands at `phase`, in figure-local
    /// units.
    const fn depth(self, phase: f64) -> f64 {
        match self {
            Self::Dodge => action::dodge_depth(phase),
            Self::Heavy => action::heavy_depth(phase),
            Self::Stagger => action::stagger_depth(phase),
            Self::Die => state::die_depth(phase),
        }
    }
}

/// The phases `keys` are keyed at.
const fn phases<const N: usize>(keys: &[Key; N]) -> [f64; N] {
    let mut out = [0.0; N];
    let mut index = 0;
    while index < N {
        out[index] = keys[index].phase;
        index += 1;
    }
    out
}

/// `sink`'s depth keyed at `phases` as root heights.
const fn sunk<const N: usize>(phases: [f64; N], sink: Sink) -> [Key; N] {
    let mut keys = [Key::new(0.0, 0.0); N];
    let mut index = 0;
    while index < N {
        keys[index] = Key::new(phases[index], rooted(-sink.depth(phases[index])));
        index += 1;
    }
    keys
}

#[cfg(test)]
#[path = "motion/tests.rs"]
mod tests;
