//! The shipped motion set: the clips a figure is actually animated by.
//!
//! Three, and each is here because a quality measurement needs it: an idle
//! for the always-on layers and for loop closure, and a walk and a run for
//! the gait, the stride and the foot slide. Combat, hit and death clips are
//! `WinterSun`'s content rather than the engine's, and are authored with the
//! game.
//!
//! # How the leg curves were arrived at
//!
//! Not by eye. Each locomotion clip states a **foot path** — how far in
//! front of the hip the foot strikes, how deep the figure stands, how high
//! the swing foot clears, and what fraction of the cycle the foot is down
//! for — and the hip, knee and ankle keys below are that path solved through
//! the same two-bone geometry the planting layer uses. What the path implies
//! is therefore what the rig does, and the crate's own tests measure the
//! stride back out of the clip and check it against the number the path was
//! authored to give.
//!
//! The pelvis sits at a fixed height, so a foot cannot travel fore and aft
//! along level ground with a straight leg: every locomotion path is authored
//! with the figure standing a stated depth into its own legs. That is why
//! the walk has a crouch at all, and why it is a number here rather than a
//! feel.
//!
//! Each clip therefore states the height it holds the body at, because the
//! articulation cannot be asked: both legs tucked is a run's flight phase
//! and a deep crouch at once. The height is its crouch wherever a foot is
//! down, and the run adds the arc its body follows over the moment it has
//! neither. `quality::grounding` measures the two halves against each other,
//! so a depth here that its keys do not produce is a failure rather than a
//! figure quietly sunk into the floor.
//!
//! The ankle levels the foot against the ground by a fixed fraction of the
//! leg's own turn rather than all of it, because a heel lifts at toe-off and
//! a knee-high swing foot hangs — neither is level, and countering the whole
//! turn would pin the ankle at its limit through half the cycle.
//!
//! # One cycle, two sides
//!
//! A left and a right limb do the same thing half a turn apart, so only one
//! side is authored and the other is that cycle rotated half a turn. Two
//! tables that must stay each other's mirror image are two things to keep in
//! step, and a walk whose sides disagreed would read as a limp nobody
//! animated.

use tairix_inline::ArrayVec;

use crate::clip::{Clip, Curve, Event, Key, Lift, Loop};
use crate::error::FigureError;
use crate::humanoid::{SHANK_LENGTH, THIGH_LENGTH};
use crate::pose::Param;
use crate::socket::Side;

/// How many curves one motion drives.
///
/// A clip carries at most one curve per parameter — [`Clip::new`] refuses a
/// second — so the parameter count is the bound rather than a number chosen
/// beside it.
pub const MAX_CURVES: usize = Param::COUNT;

/// Which shipped motion.
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
}

impl Kind {
    /// Every shipped motion.
    pub const ALL: [Self; 3] = [Self::Idle, Self::Walk, Self::Run];

    /// Its position in [`Self::ALL`].
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Idle => 0,
            Self::Walk => 1,
            Self::Run => 2,
        }
    }

    /// Its stable name, for a ledger row or a diagnostic.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Walk => "walk",
            Self::Run => "run",
        }
    }

    /// How far one cycle was authored to carry the figure, or `None` for a
    /// motion that stays where it is.
    ///
    /// The authoring intent rather than a measurement: what the foot path
    /// below was built to give. [`Gait::fitted`] measures the clip's *actual*
    /// stride, and a test holds the two together — which is what stops the
    /// path's documentation drifting from the keys it produced.
    ///
    /// [`Gait::fitted`]: crate::gait::Gait::fitted
    #[must_use]
    pub const fn stride(self) -> Option<f64> {
        match self {
            Self::Idle => None,
            Self::Walk => Some(2.0 * WALK_HALF_STEP / WALK_STANCE),
            Self::Run => Some(2.0 * RUN_HALF_STEP / RUN_STANCE),
        }
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
    seconds: f64,
    repeat: Loop,
    events: &'static [Event],
    lift: &'static [Key],
}

impl Motion {
    /// Assemble the shipped clip for `kind`.
    ///
    /// # Errors
    ///
    /// Whatever [`Curve::new`] refuses about the tables below — only
    /// reachable if one is edited into something a parameter's range does
    /// not hold, which is the point of checking it here.
    pub fn new(kind: Kind) -> Result<Self, FigureError> {
        let (seconds, events, keyed, lift): (
            f64,
            &'static [Event],
            &'static [Keyed],
            &'static [Key],
        ) = match kind {
            Kind::Idle => (IDLE_SECONDS, &NO_EVENTS, &IDLE_CURVES, &IDLE_LIFT),
            Kind::Walk => (WALK_SECONDS, &FOOTSTEPS, &WALK_CURVES, &WALK_LIFT),
            Kind::Run => (RUN_SECONDS, &FOOTSTEPS, &RUN_CURVES, &RUN_LIFT),
        };
        let mut curves = ArrayVec::new();
        for (param, keys) in keyed {
            curves
                .try_push(Curve::new(*param, keys)?)
                .map_err(|_| FigureError::DuplicateCurve)?;
        }
        Ok(Self {
            kind,
            curves,
            seconds,
            repeat: Loop::Wrap,
            events,
            lift,
        })
    }

    /// Which motion it is.
    #[must_use]
    pub const fn kind(&self) -> Kind {
        self.kind
    }

    /// The clip, borrowing this motion's curves.
    ///
    /// # Errors
    ///
    /// Whatever [`Clip::new`] refuses, which for these tables is nothing.
    pub fn clip(&self) -> Result<Clip<'_>, FigureError> {
        Clip::new(self.seconds, self.repeat, &self.curves, self.events)?
            .lifting(Lift::new(self.lift)?)
    }
}

/// Every shipped motion, held so the clips can borrow their curves.
#[derive(Clone, Debug)]
pub struct Set {
    motions: [Motion; Kind::ALL.len()],
}

impl Set {
    /// Assemble every shipped motion.
    ///
    /// # Errors
    ///
    /// As [`Motion::new`].
    pub fn new() -> Result<Self, FigureError> {
        Ok(Self {
            motions: [
                Motion::new(Kind::Idle)?,
                Motion::new(Kind::Walk)?,
                Motion::new(Kind::Run)?,
            ],
        })
    }

    /// `kind`'s clip.
    ///
    /// # Errors
    ///
    /// As [`Motion::clip`].
    pub(crate) fn clip(&self, kind: Kind) -> Result<Clip<'_>, FigureError> {
        self.motions[kind.index()].clip()
    }

    /// Every clip, where [`Kind::index`] puts it.
    ///
    /// # Errors
    ///
    /// As [`Motion::clip`].
    pub fn clips(&self) -> Result<Clips<'_>, FigureError> {
        Ok(Clips([
            self.clip(Kind::Idle)?,
            self.clip(Kind::Walk)?,
            self.clip(Kind::Run)?,
        ]))
    }
}

/// Every shipped clip, indexed by [`Kind::index`].
///
/// Only [`Set::clips`] makes one, so a machine borrowing the table cannot be
/// handed the clips in an order that plays a walk when a run was asked for.
#[derive(Copy, Clone, Debug)]
pub struct Clips<'a>([Clip<'a>; Kind::ALL.len()]);

impl<'a> Clips<'a> {
    /// The table, for a machine to borrow.
    pub(crate) const fn table(&self) -> &[Clip<'a>] {
        &self.0
    }
}

/// One parameter's shipped curve.
type Keyed = (Param, &'static [Key]);

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

/// A clip with nothing to announce.
const NO_EVENTS: [Event; 0] = [];

/// Each foot meeting the ground, at the head of its own stance.
const FOOTSTEPS: [Event; 2] = [
    Event::new("footstep_left", 0.0),
    Event::new("footstep_right", 0.5),
];

/// How long one idle cycle lasts, in seconds.
const IDLE_SECONDS: f64 = 4.0;

/// How long one walk cycle — two steps — lasts, in seconds.
const WALK_SECONDS: f64 = 1.1;

/// How long one run cycle lasts, in seconds.
const RUN_SECONDS: f64 = 0.62;

/// How far in front of the hip the walking foot strikes, in figure-local
/// units.
const WALK_HALF_STEP: f64 = 14.0;

/// What fraction of the walk cycle each foot is on the ground for.
///
/// Exactly half, so the figure is always on one foot and never on none: a
/// walk is the gait with no flight phase.
const WALK_STANCE: f64 = 0.5;

/// How far in front of the hip the running foot strikes.
const RUN_HALF_STEP: f64 = 18.0;

/// What fraction of the run cycle each foot is on the ground for.
///
/// Under a half, so there is a moment with neither foot down — which is
/// what makes it a run rather than a fast walk.
const RUN_STANCE: f64 = 0.375;

/// How deep each path stands into its own legs, in figure-local units.
///
/// The depth its leg keys were solved with, and therefore the height the
/// body sits at while a foot is down: the root sinks by exactly this so the
/// planted foot reaches the floor. `quality::grounding` measures the two
/// against each other rather than either being trusted.
const IDLE_CROUCH: f64 = 1.2;
const WALK_CROUCH: f64 = 3.5;
const RUN_CROUCH: f64 = 6.0;

/// How far the running body rises between toe-off and mid-flight.
///
/// A run has a moment with neither foot down, and where the body is then is
/// not in its articulation — both legs tucked reads identically to a deep
/// crouch. The leg keys carry no push-off of their own to imply it, so the
/// rise is stated here: a fortieth of the figure's height, which is a pixel
/// or two of lift at the largest size the desktop draws one. Enough to read
/// as a bound rather than a glide, and far inside the stride the feet are
/// pacing.
const RUN_FLIGHT_RISE: f64 = 2.5;

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

/// The run's root height `t` of the way through a flight window, `t` running
/// from `-1` at toe-off through `0` at mid-flight to `1` at the next strike.
///
/// A parabola, which is the arc a body with nothing holding it up follows;
/// it meets the stance height at both ends, so the height never steps where
/// a foot takes over.
const fn flight(t: f64) -> f64 {
    rooted(-RUN_CROUCH + RUN_FLIGHT_RISE * (1.0 - t * t))
}

/// The idle and walk hold one height throughout: both keep a foot down for
/// every phase of the cycle, so the body never leaves it.
const IDLE_LIFT: [Key; 2] = [
    Key::new(0.0, rooted(-IDLE_CROUCH)),
    Key::new(1.0, rooted(-IDLE_CROUCH)),
];

const WALK_LIFT: [Key; 2] = [
    Key::new(0.0, rooted(-WALK_CROUCH)),
    Key::new(1.0, rooted(-WALK_CROUCH)),
];

/// The run's two flight arcs, one per step, keyed on the cycle's own grid.
const RUN_LIFT: [Key; 19] = [
    Key::new(0.000_000, flight(1.0)),
    Key::new(0.375_000, flight(-1.0)),
    Key::new(0.390_625, flight(-0.75)),
    Key::new(0.406_250, flight(-0.50)),
    Key::new(0.421_875, flight(-0.25)),
    Key::new(0.437_500, flight(0.00)),
    Key::new(0.453_125, flight(0.25)),
    Key::new(0.468_750, flight(0.50)),
    Key::new(0.484_375, flight(0.75)),
    Key::new(0.500_000, flight(1.0)),
    Key::new(0.875_000, flight(-1.0)),
    Key::new(0.890_625, flight(-0.75)),
    Key::new(0.906_250, flight(-0.50)),
    Key::new(0.921_875, flight(-0.25)),
    Key::new(0.937_500, flight(0.00)),
    Key::new(0.953_125, flight(0.25)),
    Key::new(0.968_750, flight(0.50)),
    Key::new(0.984_375, flight(0.75)),
    Key::new(1.000_000, flight(1.0)),
];

/// The authored leg cycles, left side, solved from the foot paths above.
const WALK_HIP_LEFT: [Key; 33] = [
    Key::new(0.000_000, 0.276_264),
    Key::new(0.031_250, 0.279_460),
    Key::new(0.062_500, 0.277_239),
    Key::new(0.093_750, 0.271_023),
    Key::new(0.125_000, 0.261_568),
    Key::new(0.156_250, 0.249_338),
    Key::new(0.187_500, 0.234_634),
    Key::new(0.218_750, 0.217_667),
    Key::new(0.250_000, 0.198_579),
    Key::new(0.281_250, 0.177_459),
    Key::new(0.312_500, 0.154_348),
    Key::new(0.343_750, 0.129_229),
    Key::new(0.375_000, 0.102_017),
    Key::new(0.406_250, 0.072_522),
    Key::new(0.437_500, 0.040_390),
    Key::new(0.468_750, 0.004_959),
    Key::new(0.500_000, -0.117_023),
    Key::new(0.531_250, -0.175_336),
    Key::new(0.562_500, -0.080_078),
    Key::new(0.593_750, 0.029_406),
    Key::new(0.625_000, 0.093_555),
    Key::new(0.656_250, 0.161_984),
    Key::new(0.687_500, 0.230_536),
    Key::new(0.718_750, 0.294_827),
    Key::new(0.750_000, 0.349_500),
    Key::new(0.781_250, 0.389_000),
    Key::new(0.812_500, 0.409_319),
    Key::new(0.843_750, 0.409_312),
    Key::new(0.875_000, 0.390_736),
    Key::new(0.906_250, 0.357_722),
    Key::new(0.937_500, 0.317_416),
    Key::new(0.968_750, 0.283_854),
    Key::new(1.000_000, 0.276_264),
];

const WALK_KNEE_LEFT: [Key; 33] = [
    Key::new(0.000_000, 0.196_699),
    Key::new(0.031_250, 0.231_949),
    Key::new(0.062_500, 0.258_996),
    Key::new(0.093_750, 0.280_093),
    Key::new(0.125_000, 0.296_401),
    Key::new(0.156_250, 0.308_589),
    Key::new(0.187_500, 0.317_060),
    Key::new(0.218_750, 0.322_057),
    Key::new(0.250_000, 0.323_709),
    Key::new(0.281_250, 0.322_057),
    Key::new(0.312_500, 0.317_060),
    Key::new(0.343_750, 0.308_589),
    Key::new(0.375_000, 0.296_401),
    Key::new(0.406_250, 0.280_093),
    Key::new(0.437_500, 0.258_996),
    Key::new(0.468_750, 0.231_949),
    Key::new(0.500_000, 0.196_699),
    Key::new(0.531_250, 0.188_628),
    Key::new(0.562_500, 0.239_258),
    Key::new(0.593_750, 0.315_553),
    Key::new(0.625_000, 0.394_493),
    Key::new(0.656_250, 0.465_013),
    Key::new(0.687_500, 0.520_434),
    Key::new(0.718_750, 0.555_895),
    Key::new(0.750_000, 0.568_113),
    Key::new(0.781_250, 0.555_895),
    Key::new(0.812_500, 0.520_434),
    Key::new(0.843_750, 0.465_013),
    Key::new(0.875_000, 0.394_493),
    Key::new(0.906_250, 0.315_553),
    Key::new(0.937_500, 0.239_258),
    Key::new(0.968_750, 0.188_628),
    Key::new(1.000_000, 0.196_699),
];

const WALK_ANKLE_LEFT: [Key; 33] = [
    Key::new(0.000_000, 0.080_448),
    Key::new(0.031_250, 0.002_241),
    Key::new(0.062_500, -0.067_113),
    Key::new(0.093_750, -0.130_178),
    Key::new(0.125_000, -0.188_226),
    Key::new(0.156_250, -0.241_938),
    Key::new(0.187_500, -0.291_676),
    Key::new(0.218_750, -0.337_603),
    Key::new(0.250_000, -0.379_744),
    Key::new(0.281_250, -0.418_019),
    Key::new(0.312_500, -0.452_250),
    Key::new(0.343_750, -0.482_156),
    Key::new(0.375_000, -0.507_329),
    Key::new(0.406_250, -0.527_179),
    Key::new(0.437_500, -0.540_810),
    Key::new(0.468_750, -0.546_760),
    Key::new(0.500_000, -0.542_292),
    Key::new(0.531_250, -0.557_909),
    Key::new(0.562_500, -0.622_267),
    Key::new(0.593_750, -0.698_515),
    Key::new(0.625_000, -0.759_675),
    Key::new(0.656_250, -0.792_064),
    Key::new(0.687_500, -0.787_969),
    Key::new(0.718_750, -0.744_492),
    Key::new(0.750_000, -0.664_471),
    Key::new(0.781_250, -0.556_147),
    Key::new(0.812_500, -0.430_404),
    Key::new(0.843_750, -0.297_407),
    Key::new(0.875_000, -0.165_313),
    Key::new(0.906_250, -0.041_883),
    Key::new(0.937_500, 0.060_611),
    Key::new(0.968_750, 0.115_001),
    Key::new(1.000_000, 0.080_448),
];

const RUN_HIP_LEFT: [Key; 33] = [
    Key::new(0.000_000, 0.364_662),
    Key::new(0.031_250, 0.369_350),
    Key::new(0.062_500, 0.362_333),
    Key::new(0.093_750, 0.346_751),
    Key::new(0.125_000, 0.324_155),
    Key::new(0.156_250, 0.295_498),
    Key::new(0.187_500, 0.261_441),
    Key::new(0.218_750, 0.222_458),
    Key::new(0.250_000, 0.178_845),
    Key::new(0.281_250, 0.130_666),
    Key::new(0.312_500, 0.077_603),
    Key::new(0.343_750, 0.018_622),
    Key::new(0.375_000, -0.163_425),
    Key::new(0.406_250, -0.340_846),
    Key::new(0.437_500, -0.354_357),
    Key::new(0.468_750, -0.236_043),
    Key::new(0.500_000, -0.048_890),
    Key::new(0.531_250, 0.052_488),
    Key::new(0.562_500, 0.127_020),
    Key::new(0.593_750, 0.207_321),
    Key::new(0.625_000, 0.291_509),
    Key::new(0.656_250, 0.375_545),
    Key::new(0.687_500, 0.452_226),
    Key::new(0.718_750, 0.512_642),
    Key::new(0.750_000, 0.549_942),
    Key::new(0.781_250, 0.562_059),
    Key::new(0.812_500, 0.551_197),
    Key::new(0.843_750, 0.521_753),
    Key::new(0.875_000, 0.478_971),
    Key::new(0.906_250, 0.429_349),
    Key::new(0.937_500, 0.383_758),
    Key::new(0.968_750, 0.360_187),
    Key::new(1.000_000, 0.364_662),
];

const RUN_KNEE_LEFT: [Key; 33] = [
    Key::new(0.000_000, 0.257_372),
    Key::new(0.031_250, 0.316_239),
    Key::new(0.062_500, 0.358_478),
    Key::new(0.093_750, 0.388_915),
    Key::new(0.125_000, 0.409_674),
    Key::new(0.156_250, 0.421_802),
    Key::new(0.187_500, 0.425_795),
    Key::new(0.218_750, 0.421_802),
    Key::new(0.250_000, 0.409_674),
    Key::new(0.281_250, 0.388_915),
    Key::new(0.312_500, 0.358_478),
    Key::new(0.343_750, 0.316_239),
    Key::new(0.375_000, 0.257_372),
    Key::new(0.406_250, 0.210_371),
    Key::new(0.437_500, 0.226_273),
    Key::new(0.468_750, 0.292_293),
    Key::new(0.500_000, 0.378_270),
    Key::new(0.531_250, 0.467_397),
    Key::new(0.562_500, 0.551_374),
    Key::new(0.593_750, 0.624_649),
    Key::new(0.625_000, 0.682_259),
    Key::new(0.656_250, 0.719_426),
    Key::new(0.687_500, 0.732_320),
    Key::new(0.718_750, 0.719_426),
    Key::new(0.750_000, 0.682_259),
    Key::new(0.781_250, 0.624_649),
    Key::new(0.812_500, 0.551_374),
    Key::new(0.843_750, 0.467_397),
    Key::new(0.875_000, 0.378_270),
    Key::new(0.906_250, 0.292_293),
    Key::new(0.937_500, 0.226_273),
    Key::new(0.968_750, 0.210_371),
    Key::new(1.000_000, 0.257_372),
];

const RUN_ANKLE_LEFT: [Key; 33] = [
    Key::new(0.000_000, 0.079_737),
    Key::new(0.031_250, -0.014_481),
    Key::new(0.062_500, -0.096_915),
    Key::new(0.093_750, -0.171_353),
    Key::new(0.125_000, -0.239_219),
    Key::new(0.156_250, -0.300_948),
    Key::new(0.187_500, -0.356_448),
    Key::new(0.218_750, -0.405_292),
    Key::new(0.250_000, -0.446_805),
    Key::new(0.281_250, -0.480_045),
    Key::new(0.312_500, -0.503_673),
    Key::new(0.343_750, -0.515_521),
    Key::new(0.375_000, -0.511_248),
    Key::new(0.406_250, -0.506_713),
    Key::new(0.437_500, -0.539_764),
    Key::new(0.468_750, -0.602_235),
    Key::new(0.500_000, -0.669_415),
    Key::new(0.531_250, -0.726_270),
    Key::new(0.562_500, -0.763_756),
    Key::new(0.593_750, -0.774_654),
    Key::new(0.625_000, -0.753_146),
    Key::new(0.656_250, -0.696_809),
    Key::new(0.687_500, -0.609_369),
    Key::new(0.718_750, -0.500_957),
    Key::new(0.750_000, -0.383_956),
    Key::new(0.781_250, -0.267_885),
    Key::new(0.812_500, -0.157_789),
    Key::new(0.843_750, -0.055_891),
    Key::new(0.875_000, 0.035_782),
    Key::new(0.906_250, 0.112_282),
    Key::new(0.937_500, 0.160_329),
    Key::new(0.968_750, 0.153_917),
    Key::new(1.000_000, 0.079_737),
];

const WALK_ARM_LEFT: [Key; 17] = [
    Key::new(0.000_000, -0.220_000),
    Key::new(0.062_500, -0.203_253),
    Key::new(0.125_000, -0.155_563),
    Key::new(0.187_500, -0.084_190),
    Key::new(0.250_000, 0.000_000),
    Key::new(0.312_500, 0.084_190),
    Key::new(0.375_000, 0.155_563),
    Key::new(0.437_500, 0.203_253),
    Key::new(0.500_000, 0.220_000),
    Key::new(0.562_500, 0.203_253),
    Key::new(0.625_000, 0.155_563),
    Key::new(0.687_500, 0.084_190),
    Key::new(0.750_000, 0.000_000),
    Key::new(0.812_500, -0.084_190),
    Key::new(0.875_000, -0.155_563),
    Key::new(0.937_500, -0.203_253),
    Key::new(1.000_000, -0.220_000),
];
const WALK_ELBOW_LEFT: [Key; 17] = [
    Key::new(0.000_000, 0.120_000),
    Key::new(0.062_500, 0.123_045),
    Key::new(0.125_000, 0.131_716),
    Key::new(0.187_500, 0.144_693),
    Key::new(0.250_000, 0.160_000),
    Key::new(0.312_500, 0.175_307),
    Key::new(0.375_000, 0.188_284),
    Key::new(0.437_500, 0.196_955),
    Key::new(0.500_000, 0.200_000),
    Key::new(0.562_500, 0.196_955),
    Key::new(0.625_000, 0.188_284),
    Key::new(0.687_500, 0.175_307),
    Key::new(0.750_000, 0.160_000),
    Key::new(0.812_500, 0.144_693),
    Key::new(0.875_000, 0.131_716),
    Key::new(0.937_500, 0.123_045),
    Key::new(1.000_000, 0.120_000),
];
const WALK_TWIST: [Key; 17] = [
    Key::new(0.000_000, -0.100_000),
    Key::new(0.062_500, -0.092_388),
    Key::new(0.125_000, -0.070_711),
    Key::new(0.187_500, -0.038_268),
    Key::new(0.250_000, 0.000_000),
    Key::new(0.312_500, 0.038_268),
    Key::new(0.375_000, 0.070_711),
    Key::new(0.437_500, 0.092_388),
    Key::new(0.500_000, 0.100_000),
    Key::new(0.562_500, 0.092_388),
    Key::new(0.625_000, 0.070_711),
    Key::new(0.687_500, 0.038_268),
    Key::new(0.750_000, 0.000_000),
    Key::new(0.812_500, -0.038_268),
    Key::new(0.875_000, -0.070_711),
    Key::new(0.937_500, -0.092_388),
    Key::new(1.000_000, -0.100_000),
];
const RUN_ARM_LEFT: [Key; 17] = [
    Key::new(0.000_000, -0.280_000),
    Key::new(0.062_500, -0.258_686),
    Key::new(0.125_000, -0.197_990),
    Key::new(0.187_500, -0.107_151),
    Key::new(0.250_000, 0.000_000),
    Key::new(0.312_500, 0.107_151),
    Key::new(0.375_000, 0.197_990),
    Key::new(0.437_500, 0.258_686),
    Key::new(0.500_000, 0.280_000),
    Key::new(0.562_500, 0.258_686),
    Key::new(0.625_000, 0.197_990),
    Key::new(0.687_500, 0.107_151),
    Key::new(0.750_000, 0.000_000),
    Key::new(0.812_500, -0.107_151),
    Key::new(0.875_000, -0.197_990),
    Key::new(0.937_500, -0.258_686),
    Key::new(1.000_000, -0.280_000),
];
const RUN_ELBOW_LEFT: [Key; 17] = [
    Key::new(0.000_000, 0.450_000),
    Key::new(0.062_500, 0.453_806),
    Key::new(0.125_000, 0.464_645),
    Key::new(0.187_500, 0.480_866),
    Key::new(0.250_000, 0.500_000),
    Key::new(0.312_500, 0.519_134),
    Key::new(0.375_000, 0.535_355),
    Key::new(0.437_500, 0.546_194),
    Key::new(0.500_000, 0.550_000),
    Key::new(0.562_500, 0.546_194),
    Key::new(0.625_000, 0.535_355),
    Key::new(0.687_500, 0.519_134),
    Key::new(0.750_000, 0.500_000),
    Key::new(0.812_500, 0.480_866),
    Key::new(0.875_000, 0.464_645),
    Key::new(0.937_500, 0.453_806),
    Key::new(1.000_000, 0.450_000),
];
const RUN_TWIST: [Key; 17] = [
    Key::new(0.000_000, -0.180_000),
    Key::new(0.062_500, -0.166_298),
    Key::new(0.125_000, -0.127_279),
    Key::new(0.187_500, -0.068_883),
    Key::new(0.250_000, 0.000_000),
    Key::new(0.312_500, 0.068_883),
    Key::new(0.375_000, 0.127_279),
    Key::new(0.437_500, 0.166_298),
    Key::new(0.500_000, 0.180_000),
    Key::new(0.562_500, 0.166_298),
    Key::new(0.625_000, 0.127_279),
    Key::new(0.687_500, 0.068_883),
    Key::new(0.750_000, 0.000_000),
    Key::new(0.812_500, -0.068_883),
    Key::new(0.875_000, -0.127_279),
    Key::new(0.937_500, -0.166_298),
    Key::new(1.000_000, -0.180_000),
];
const IDLE_TILT: [Key; 17] = [
    Key::new(0.000_000, 0.000_000),
    Key::new(0.062_500, 0.019_134),
    Key::new(0.125_000, 0.035_355),
    Key::new(0.187_500, 0.046_194),
    Key::new(0.250_000, 0.050_000),
    Key::new(0.312_500, 0.046_194),
    Key::new(0.375_000, 0.035_355),
    Key::new(0.437_500, 0.019_134),
    Key::new(0.500_000, 0.000_000),
    Key::new(0.562_500, -0.019_134),
    Key::new(0.625_000, -0.035_355),
    Key::new(0.687_500, -0.046_194),
    Key::new(0.750_000, -0.050_000),
    Key::new(0.812_500, -0.046_194),
    Key::new(0.875_000, -0.035_355),
    Key::new(0.937_500, -0.019_134),
    Key::new(1.000_000, 0.000_000),
];
const IDLE_HEAD: [Key; 17] = [
    Key::new(0.000_000, 0.060_000),
    Key::new(0.062_500, 0.055_433),
    Key::new(0.125_000, 0.042_426),
    Key::new(0.187_500, 0.022_961),
    Key::new(0.250_000, 0.000_000),
    Key::new(0.312_500, -0.022_961),
    Key::new(0.375_000, -0.042_426),
    Key::new(0.437_500, -0.055_433),
    Key::new(0.500_000, -0.060_000),
    Key::new(0.562_500, -0.055_433),
    Key::new(0.625_000, -0.042_426),
    Key::new(0.687_500, -0.022_961),
    Key::new(0.750_000, 0.000_000),
    Key::new(0.812_500, 0.022_961),
    Key::new(0.875_000, 0.042_426),
    Key::new(0.937_500, 0.055_433),
    Key::new(1.000_000, 0.060_000),
];

/// The relaxed stance the idle legs hold, solved once for a figure standing
/// a little into its own knees.
const IDLE_HIP_KEY: Key = Key::new(0.000_000, 0.115_706);
const IDLE_KNEE_KEY: Key = Key::new(0.000_000, 0.188_757);
const IDLE_ANKLE_KEY: Key = Key::new(0.000_000, -0.316_579);

/// The right leg, half a cycle behind the left.
const WALK_HIP_RIGHT: [Key; 33] = opposite(&WALK_HIP_LEFT);
const WALK_KNEE_RIGHT: [Key; 33] = opposite(&WALK_KNEE_LEFT);
const WALK_ANKLE_RIGHT: [Key; 33] = opposite(&WALK_ANKLE_LEFT);
const RUN_HIP_RIGHT: [Key; 33] = opposite(&RUN_HIP_LEFT);
const RUN_KNEE_RIGHT: [Key; 33] = opposite(&RUN_KNEE_LEFT);
const RUN_ANKLE_RIGHT: [Key; 33] = opposite(&RUN_ANKLE_LEFT);

/// The right arm, half a cycle behind the left — which is the same thing as
/// saying it swings with the left leg.
const WALK_ARM_RIGHT: [Key; 17] = opposite(&WALK_ARM_LEFT);
const WALK_ELBOW_RIGHT: [Key; 17] = opposite(&WALK_ELBOW_LEFT);
const RUN_ARM_RIGHT: [Key; 17] = opposite(&RUN_ARM_LEFT);
const RUN_ELBOW_RIGHT: [Key; 17] = opposite(&RUN_ELBOW_LEFT);

/// Standing: the legs solved once for a relaxed, slightly-sunk stance, so
/// both sides read the same table.
const IDLE_HIP: [Key; 1] = [IDLE_HIP_KEY];
const IDLE_KNEE: [Key; 1] = [IDLE_KNEE_KEY];
const IDLE_ANKLE: [Key; 1] = [IDLE_ANKLE_KEY];

/// Arms hanging clear of the trunk rather than through it.
const IDLE_SPLAY: [Key; 1] = [Key::new(0.0, 0.05)];

/// A trace of bend, because a hanging arm is not a plank.
const IDLE_ELBOW: [Key; 1] = [Key::new(0.0, 0.07)];

/// Walking leans in a little; running leans in a lot.
const WALK_LEAN: [Key; 1] = [Key::new(0.0, 0.06)];
const RUN_LEAN: [Key; 1] = [Key::new(0.0, 0.20)];

/// The trunk counter-rotating against the pelvis, in phase with the arms.
const IDLE_CURVES: [Keyed; 12] = [
    (Param::SpineTilt, &IDLE_TILT),
    (Param::HeadTurn, &IDLE_HEAD),
    (Param::ShoulderSplay(Side::Left), &IDLE_SPLAY),
    (Param::ShoulderSplay(Side::Right), &IDLE_SPLAY),
    (Param::ElbowBend(Side::Left), &IDLE_ELBOW),
    (Param::ElbowBend(Side::Right), &IDLE_ELBOW),
    (Param::HipSwing(Side::Left), &IDLE_HIP),
    (Param::HipSwing(Side::Right), &IDLE_HIP),
    (Param::KneeBend(Side::Left), &IDLE_KNEE),
    (Param::KneeBend(Side::Right), &IDLE_KNEE),
    (Param::AnkleAngle(Side::Left), &IDLE_ANKLE),
    (Param::AnkleAngle(Side::Right), &IDLE_ANKLE),
];

const WALK_CURVES: [Keyed; 12] = [
    (Param::SpineBend, &WALK_LEAN),
    (Param::SpineTwist, &WALK_TWIST),
    (Param::ShoulderSwing(Side::Left), &WALK_ARM_LEFT),
    (Param::ShoulderSwing(Side::Right), &WALK_ARM_RIGHT),
    (Param::ElbowBend(Side::Left), &WALK_ELBOW_LEFT),
    (Param::ElbowBend(Side::Right), &WALK_ELBOW_RIGHT),
    (Param::HipSwing(Side::Left), &WALK_HIP_LEFT),
    (Param::HipSwing(Side::Right), &WALK_HIP_RIGHT),
    (Param::KneeBend(Side::Left), &WALK_KNEE_LEFT),
    (Param::KneeBend(Side::Right), &WALK_KNEE_RIGHT),
    (Param::AnkleAngle(Side::Left), &WALK_ANKLE_LEFT),
    (Param::AnkleAngle(Side::Right), &WALK_ANKLE_RIGHT),
];

const RUN_CURVES: [Keyed; 12] = [
    (Param::SpineBend, &RUN_LEAN),
    (Param::SpineTwist, &RUN_TWIST),
    (Param::ShoulderSwing(Side::Left), &RUN_ARM_LEFT),
    (Param::ShoulderSwing(Side::Right), &RUN_ARM_RIGHT),
    (Param::ElbowBend(Side::Left), &RUN_ELBOW_LEFT),
    (Param::ElbowBend(Side::Right), &RUN_ELBOW_RIGHT),
    (Param::HipSwing(Side::Left), &RUN_HIP_LEFT),
    (Param::HipSwing(Side::Right), &RUN_HIP_RIGHT),
    (Param::KneeBend(Side::Left), &RUN_KNEE_LEFT),
    (Param::KneeBend(Side::Right), &RUN_KNEE_RIGHT),
    (Param::AnkleAngle(Side::Left), &RUN_ANKLE_LEFT),
    (Param::AnkleAngle(Side::Right), &RUN_ANKLE_RIGHT),
];

#[cfg(test)]
#[path = "motion/tests.rs"]
mod tests;
