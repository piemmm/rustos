//! What Cinder decides to do: three needs, a closed set of intents, and a
//! seeded generator so every decision is reproducible.
//!
//! Deliberately small. There is no feeding economy, no breeding, no inventory,
//! and no stat the user has to manage: Cinder is a companion, and a companion
//! that becomes a chore has stopped being one. The needs exist to make the
//! creature's behaviour legible — you can tell at a glance that he is tired or
//! that he wants playing with — not to be optimised.

use tairix_rng::RandU64;
use tairix_util::mathf;

/// How Cinder is feeling, each from `0.0` (spent) to `1.0` (full).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Needs {
    /// Falls while active, recovers while napping.
    pub energy: f64,
    /// Falls while idle, recovers by chasing and pouncing.
    pub play: f64,
    /// Falls slowly all the time, recovers when petted.
    pub affection: f64,
}

impl Default for Needs {
    fn default() -> Self {
        // A freshly woken companion is rested and sociable, but with something
        // left to want: a creature with nothing to want does nothing.
        Self {
            energy: 0.9,
            play: 0.55,
            affection: 0.6,
        }
    }
}

impl Needs {
    /// Advance the needs by `dt` seconds of `activity`.
    pub fn tick(&mut self, dt: f64, activity: Activity) {
        let (energy, play, affection) = activity.drift();
        self.energy = clamp_unit(self.energy + energy * dt);
        self.play = clamp_unit(self.play + play * dt);
        self.affection = clamp_unit(self.affection + affection * dt);
    }

    /// Record a pet: affection up sharply, and a little play with it.
    pub fn petted(&mut self) {
        self.affection = clamp_unit(self.affection + PET_AFFECTION);
        self.play = clamp_unit(self.play + PET_PLAY);
    }

    /// How cheerful Cinder is, from `0.0` to `1.0`.
    ///
    /// What the face shows: a well-played-with, well-petted companion grins,
    /// and one who has been left alone does not. Energy is deliberately not
    /// in it — being tired is sleepiness, which the eyes say, not unhappiness.
    #[must_use]
    pub fn cheer(&self) -> f64 {
        f64::midpoint(self.play, self.affection)
    }

    /// The need that is most pressing, or `None` when nothing is.
    ///
    /// One answer, so the intent chooser cannot weigh two needs differently
    /// from the way the pen's mood readout reports them.
    #[must_use]
    pub fn most_pressing(&self) -> Option<Need> {
        let worst = [
            (Need::Rest, self.energy),
            (Need::Play, self.play),
            (Need::Company, self.affection),
        ]
        .into_iter()
        .filter(|&(_, level)| level < PRESSING_BELOW)
        .reduce(|a, b| if a.1 <= b.1 { a } else { b });
        worst.map(|(need, _)| need)
    }
}

/// Which need is pressing.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Need {
    /// Tired.
    Rest,
    /// Bored.
    Play,
    /// Lonely.
    Company,
}

/// Below this a need counts as pressing.
const PRESSING_BELOW: f64 = 0.3;

/// How much one pet restores.
const PET_AFFECTION: f64 = 0.22;

/// How much play one pet restores.
const PET_PLAY: f64 = 0.08;

/// What Cinder is doing, for the purpose of how the needs drift.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Activity {
    /// Still: play drains, energy holds.
    Resting,
    /// Moving about: energy drains slowly.
    Ambling,
    /// Running, pouncing, climbing: energy drains, play recovers.
    Exerting,
    /// Asleep: energy recovers.
    Sleeping,
}

impl Activity {
    /// Per-second drift of energy, play, and affection.
    const fn drift(self) -> (f64, f64, f64) {
        match self {
            Self::Resting => (0.004, -0.010, -0.0035),
            Self::Ambling => (-0.006, -0.004, -0.0035),
            Self::Exerting => (-0.022, 0.028, -0.0035),
            Self::Sleeping => (0.030, -0.002, -0.0035),
        }
    }
}

/// What Cinder means to do next.
///
/// A closed set: every one is something the creature visibly does, and there
/// is no "idle" catch-all that would let a bug read as a mood.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Intent {
    /// Walk somewhere for no particular reason.
    Wander,
    /// Sit and look about.
    Sit,
    /// Groom: sit and work at the coat.
    Groom,
    /// Curl up and sleep.
    Nap,
    /// Trot towards the pointer.
    Chase,
    /// Leap at the pointer, having caught up with it.
    Pounce,
    /// Climb onto the window ahead.
    Climb,
    /// Flatten and slip under the window ahead.
    Burrow,
    /// Head back to the playpen.
    ComeHome,
}

impl Intent {
    /// The activity this intent counts as, for the needs' drift.
    #[must_use]
    pub const fn activity(self) -> Activity {
        match self {
            Self::Sit | Self::Groom => Activity::Resting,
            Self::Nap => Activity::Sleeping,
            Self::Wander | Self::ComeHome | Self::Burrow => Activity::Ambling,
            Self::Chase | Self::Pounce | Self::Climb => Activity::Exerting,
        }
    }

    /// How fast Cinder moves pursuing this intent, as a fraction of a run.
    #[must_use]
    pub const fn pace(self) -> f64 {
        match self {
            Self::Sit | Self::Groom | Self::Nap => 0.0,
            Self::Wander => 0.35,
            Self::Burrow => 0.45,
            Self::ComeHome => 0.6,
            Self::Climb => 0.7,
            Self::Chase | Self::Pounce => 1.0,
        }
    }
}

/// How long Cinder holds an intent before reconsidering, in seconds.
///
/// Long enough that the creature does not dither, short enough that it
/// answers a pointer that arrives mid-nap within a moment.
pub const RECONSIDER_SECONDS: f64 = 2.5;

/// How near the pointer has to be, in ground pixels, before a chase becomes a
/// pounce.
pub const POUNCE_RANGE: f64 = 46.0;

/// How near the pointer has to be before Cinder notices it at all.
///
/// A bound as much as a behaviour: a companion that reacted to every pointer
/// movement anywhere on a large screen would never settle.
pub const NOTICE_RANGE: f64 = 420.0;

/// The mind: the needs, the current intent, and the generator every choice is
/// drawn from.
///
/// The generator is **injected**, so a test seeds it and gets the same
/// creature every run — which is what makes behaviour something that can be
/// asserted rather than watched.
pub struct Mind<R: RandU64> {
    needs: Needs,
    intent: Intent,
    held_for: f64,
    rng: R,
}

impl<R: RandU64> Mind<R> {
    /// A mind with default needs, drawing from `rng`.
    pub fn new(rng: R) -> Self {
        Self {
            needs: Needs::default(),
            intent: Intent::Sit,
            held_for: 0.0,
            rng,
        }
    }

    /// A mind resuming from `needs`.
    pub fn resuming(rng: R, needs: Needs) -> Self {
        Self {
            needs,
            intent: Intent::Sit,
            held_for: 0.0,
            rng,
        }
    }

    /// The current needs.
    #[must_use]
    pub const fn needs(&self) -> Needs {
        self.needs
    }

    /// What Cinder means to do.
    #[must_use]
    pub const fn intent(&self) -> Intent {
        self.intent
    }

    /// Record a pet.
    pub fn petted(&mut self) {
        self.needs.petted();
        // Being petted interrupts whatever he was doing: that is the point of
        // it, and a creature that ignored a hand would not read as a pet.
        self.intent = Intent::Sit;
        self.held_for = 0.0;
    }

    /// Advance `dt` seconds, with `pointer` the ground distance to the pointer
    /// (`None` when it is out of range or there is none), and `blocked` saying
    /// a window stands in the way.
    ///
    /// Answers the intent to pursue now.
    pub fn tick(&mut self, dt: f64, pointer: Option<f64>, blocked: bool) -> Intent {
        self.needs.tick(dt, self.intent.activity());
        self.held_for += dt;

        // A pointer inside pouncing range overrides the held intent at once:
        // waiting out the reconsider timer would make the creature look like
        // it had not noticed.
        if let Some(distance) = pointer {
            if distance <= POUNCE_RANGE && self.needs.energy > SPENT {
                return self.adopt(Intent::Pounce);
            }
        }
        if self.held_for < RECONSIDER_SECONDS && !self.finished_transient() {
            return self.intent;
        }
        let chosen = self.choose(pointer, blocked);
        self.adopt(chosen)
    }

    /// Whether the held intent is one that ends of its own accord and has.
    ///
    /// A pounce is over in a moment; holding it for the full reconsider window
    /// would leave the creature frozen mid-leap.
    fn finished_transient(&self) -> bool {
        matches!(self.intent, Intent::Pounce) && self.held_for > POUNCE_SECONDS
    }

    /// Adopt `intent`, resetting the hold timer.
    fn adopt(&mut self, intent: Intent) -> Intent {
        if intent != self.intent {
            self.intent = intent;
            self.held_for = 0.0;
        }
        self.intent
    }

    /// Choose an intent from the needs, the pointer, and a draw.
    fn choose(&mut self, pointer: Option<f64>, blocked: bool) -> Intent {
        // Exhaustion wins outright: a creature that kept playing at zero
        // energy would never read as tired.
        if self.needs.energy <= SPENT {
            return Intent::Nap;
        }
        if blocked {
            // Over or under is the route planner's decision, not the mind's;
            // the mind only says "deal with what is in the way".
            return if self.draw_unit() < CLIMB_SHARE {
                Intent::Climb
            } else {
                Intent::Burrow
            };
        }
        if let Some(distance) = pointer {
            if distance <= NOTICE_RANGE {
                // A bored or lonely creature goes for the pointer readily; a
                // contented one only sometimes.
                let eagerness = 1.0 - f64::midpoint(self.needs.play, self.needs.affection);
                if self.draw_unit() < mathf::clamp(eagerness + CHASE_BASE, 0.0, 1.0) {
                    return Intent::Chase;
                }
            }
        }
        match self.needs.most_pressing() {
            Some(Need::Rest) => Intent::Nap,
            Some(Need::Play) => Intent::Wander,
            Some(Need::Company) => Intent::ComeHome,
            None => {
                // Contented: mostly potter about, sometimes settle.
                let draw = self.draw_unit();
                if draw < 0.5 {
                    Intent::Wander
                } else if draw < 0.75 {
                    Intent::Sit
                } else {
                    Intent::Groom
                }
            }
        }
    }

    /// A draw in `0.0..1.0` from the injected generator.
    fn draw_unit(&mut self) -> f64 {
        // The top 53 bits are the mantissa's width, so this is a uniform draw
        // with no rounding bias and no division by a power that is not exact.
        let bits = self.rng.next_u64() >> 11;
        // `bits` is below 2^53, which every f64 represents exactly.
        #[allow(clippy::cast_precision_loss)]
        {
            bits as f64 / (1u64 << 53) as f64
        }
    }

    /// A heading drawn uniformly from the whole circle, for a wander.
    pub fn draw_heading(&mut self) -> f64 {
        self.draw_unit() * core::f64::consts::TAU
    }

    /// How far to wander, in ground pixels.
    pub fn draw_wander_distance(&mut self) -> f64 {
        WANDER_MIN + self.draw_unit() * (WANDER_MAX - WANDER_MIN)
    }
}

/// At or below this energy Cinder is spent and will sleep.
const SPENT: f64 = 0.06;

/// How long a pounce lasts before the mind reconsiders.
const POUNCE_SECONDS: f64 = 0.8;

/// How often a blocked creature goes over rather than under.
const CLIMB_SHARE: f64 = 0.55;

/// The chance of chasing even a pointer a contented creature could ignore.
const CHASE_BASE: f64 = 0.15;

/// The shortest wander.
const WANDER_MIN: f64 = 60.0;

/// The longest wander.
const WANDER_MAX: f64 = 260.0;

/// Clamp into the unit interval the needs live in.
fn clamp_unit(value: f64) -> f64 {
    mathf::clamp(value, 0.0, 1.0)
}

#[cfg(test)]
#[path = "mind_tests.rs"]
mod tests;
