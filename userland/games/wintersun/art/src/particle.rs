//! The particle vocabulary, and the bounded field that holds one.
//!
//! Rain, snow, embers, smoke, dust, sparks, splashes and blown leaves are
//! one mechanism with ten parameter sets, not ten systems. A kind says
//! how long its particles live, how heavily they fall, how hard the wind
//! pushes them, how they are tinted and how they fade; everything else is
//! shared.
//!
//! # Budget, not a constant
//!
//! How many particles a field may hold is derived from the area on screen
//! and the machine's current memory-pressure band, never from a number
//! picked here. A wide view in a blizzard on a machine with headroom
//! carries far more than a small window on a machine under pressure, and
//! neither is a configuration — it is the same policy reading a different
//! machine.
//!
//! Particle density is also the *first* thing the frame's degradation
//! ladder sheds, so this budget is the knob an overrunning frame turns
//! before anything else gives way.
//!
//! # Retiring, and why the oldest goes
//!
//! A field at its budget that is asked for another particle retires its
//! oldest. Refusing instead would make a heavy emitter — the one at the
//! centre of what the player is looking at — starve behind a light one
//! that happened to fill the field first. Age is the fair thing to drop:
//! the oldest particle is the one nearest its own death anyway.
//!
//! "Retire the oldest, take the newest" is a queue, so the store is one.
//! A vector shifting every element down on each emission would be
//! quadratic in the budget exactly when the field is full — which is the
//! steady state of a storm, every frame — and the budget is derived from
//! the screen, so it grows with the machine that can least afford it.

use alloc::collections::VecDeque;

use tairix_raster::color::Color;
use tairix_reclaim::PressureBand;
use tairix_wintersun_net::value::{WorldPoint, WorldVector};

use crate::error::ArtError;
use crate::noise::{self, Field, Tiled};
use crate::palette::{self, Ramp};

/// What a particle is.
///
/// A closed vocabulary. Adding a kind is adding a variant with its
/// parameter row and its test, which is the same discipline the game's
/// content documents are held to — there is no data path that invents one.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(u8)]
pub enum ParticleKind {
    /// Falling rain, streaked along its travel.
    Rain = 0,
    /// Half-frozen rain: slower, and blown further.
    Sleet = 1,
    /// Snow, which the wind owns more than gravity does.
    Snow = 2,
    /// Hail: fast, heavy, and barely deflected.
    Hail = 3,
    /// Embers rising from a fire.
    Ember = 4,
    /// Smoke, which rises and spreads.
    Smoke = 5,
    /// Dust kicked up from dry ground.
    Dust = 6,
    /// A spark struck from an impact.
    Spark = 7,
    /// Water thrown up by something entering it.
    Splash = 8,
    /// A leaf or needle carried off a tree.
    Leaf = 9,
}

impl ParticleKind {
    /// Every kind, in discriminant order.
    pub const ALL: [Self; 10] = [
        Self::Rain,
        Self::Sleet,
        Self::Snow,
        Self::Hail,
        Self::Ember,
        Self::Smoke,
        Self::Dust,
        Self::Spark,
        Self::Splash,
        Self::Leaf,
    ];
}

/// How a kind of particle behaves.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ParticleParams {
    /// The ramp its colour is drawn from.
    pub ramp: Ramp,
    /// Ticks it lives for.
    pub life: u16,
    /// Southward acceleration per tick, in sub-units per tick. Negative
    /// for anything that rises.
    pub gravity: i16,
    /// How hard the wind pushes it, out of 255.
    pub drift: u8,
    /// How much of its speed it keeps each tick, out of 255.
    pub drag: u8,
    /// Its drawn radius, in sub-units.
    pub radius: u16,
    /// Alpha at birth, out of 255.
    pub opacity: u8,
}

/// The parameter set for every kind.
#[must_use]
#[rustfmt::skip]
pub const fn params(kind: ParticleKind) -> ParticleParams {
    //                                    ramp             life  grav  drift  drag  radius  alpha
    match kind {
        ParticleKind::Rain   => ParticleParams::new(palette::RAIN,      40,  180,  60, 250,  24, 150),
        ParticleKind::Sleet  => ParticleParams::new(palette::RAIN,      60,  110, 130, 244,  30, 170),
        ParticleKind::Snow   => ParticleParams::new(palette::SNOWFALL, 140,   22, 220, 232,  38, 210),
        ParticleKind::Hail   => ParticleParams::new(palette::SNOWFALL,  34,  240,  25, 252,  34, 230),
        ParticleKind::Ember  => ParticleParams::new(palette::EMBER,     90,  -34, 150, 238,  16, 255),
        ParticleKind::Smoke  => ParticleParams::new(palette::SMOKE,    220,  -12, 190, 226,  90,  90),
        ParticleKind::Dust   => ParticleParams::new(palette::DUST,     110,    8, 200, 228,  56, 110),
        ParticleKind::Spark  => ParticleParams::new(palette::EMBER,     22,  120,  40, 220,  10, 255),
        ParticleKind::Splash => ParticleParams::new(palette::SPLASH,    30,  200,  70, 246,  20, 190),
        ParticleKind::Leaf   => ParticleParams::new(palette::LEAF,     180,   26, 235, 236,  44, 235),
    }
}

impl ParticleParams {
    /// A parameter set from its fields in declaration order.
    ///
    /// Positional so the ten-row table above reads as a table, where a
    /// column can be compared down the set.
    const fn new(
        ramp: Ramp,
        life: u16,
        gravity: i16,
        drift: u8,
        drag: u8,
        radius: u16,
        opacity: u8,
    ) -> Self {
        Self {
            ramp,
            life,
            gravity,
            drift,
            drag,
            radius,
            opacity,
        }
    }
}

/// One live particle.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Particle {
    /// What it is.
    pub kind: ParticleKind,
    /// Where it is, in world sub-units.
    pub at: WorldPoint,
    /// How fast it is going, in sub-units per tick.
    pub velocity: WorldVector,
    /// Ticks it has lived.
    pub age: u16,
    /// Its own draw from the kind's ramp and radius, so a field of snow
    /// is not a field of identical flakes.
    pub variation: u8,
}

impl Particle {
    /// How far through its life it is, out of 255.
    #[must_use]
    pub fn spent(&self) -> u8 {
        let life = u32::from(params(self.kind).life).max(1);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the quotient is capped at 255 before the cast"
        )]
        {
            ((u32::from(self.age) * 255) / life).min(255) as u8
        }
    }

    /// Whether it has lived out its kind's life.
    #[must_use]
    pub fn expired(&self) -> bool {
        self.age >= params(self.kind).life
    }

    /// Its colour now: its own draw along its kind's ramp, faded out over
    /// the last part of its life.
    #[must_use]
    pub fn color(&self) -> Color {
        let p = params(self.kind);
        let mut color = p.ramp.sample(self.variation);
        let spent = self.spent();
        let remaining = u32::from(255 - spent);
        // Fading only over the tail keeps a particle at full strength
        // while it is doing its job and stops it popping when it goes.
        let alpha = if spent < FADE_FROM {
            u32::from(p.opacity)
        } else {
            u32::from(p.opacity) * remaining / u32::from(255 - FADE_FROM).max(1)
        };
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a fraction of an opacity is below it, and it is a u8"
        )]
        {
            color.a = alpha.min(255) as u8;
        }
        color
    }

    /// Its drawn radius now, in sub-units: its kind's radius, varied.
    #[must_use]
    pub fn radius(&self) -> u16 {
        let base = u32::from(params(self.kind).radius);
        // Between half and one and a half of the kind's radius.
        let scaled = base * (128 + u32::from(self.variation)) / 256;
        #[allow(
            clippy::cast_possible_truncation,
            reason = "at most one and a half times a u16 radius, itself well below the ceiling"
        )]
        {
            scaled.min(u32::from(u16::MAX)) as u16
        }
    }
}

/// How far through a life the fade starts, out of 255.
const FADE_FROM: u8 = 160;

/// Sub-units of field area one particle is budgeted for at
/// [`PressureBand::Normal`].
///
/// Not a count: the count is this divided into the area actually on
/// screen, so a wider view carries proportionally more and a narrow one
/// fewer, on the same policy.
const AREA_PER_PARTICLE: u64 = 1 << 18;

/// The most particles a field will hold whatever the view and the band.
///
/// A containment bound rather than a capacity: it bounds how much one
/// scene can ask the allocator for, so a camera pulled back over a huge
/// area cannot turn a weather effect into a memory-exhaustion path.
pub const MAX_PARTICLES: usize = 16_384;

/// The particle count a view of `area` sub-units earns under `band`.
///
/// Density is the first rung of the frame's degradation ladder, and a
/// tightening memory band turns the same knob — so a machine under
/// pressure and a machine dropping frames shed in the same way.
#[must_use]
pub fn budget(area_sub_units: u64, band: PressureBand) -> usize {
    // Straight off the band's own depth rather than a table, so a band
    // added to the model needs no row here and cannot be forgotten.
    // Critical is the deepest, so it earns nothing: an unreported gauge
    // reads as critical, and a process that never wired the pressure
    // protocol therefore carries no weather rather than all of it.
    let deepest = u64::from(PressureBand::Critical.depth()).max(1);
    let share = deepest - u64::from(band.depth());
    let count = (area_sub_units / AREA_PER_PARTICLE).saturating_mul(share) / deepest;
    usize::try_from(count)
        .unwrap_or(MAX_PARTICLES)
        .min(MAX_PARTICLES)
}

/// A bounded, budgeted set of live particles, oldest first.
#[derive(Debug, Default)]
pub struct ParticleField {
    particles: VecDeque<Particle>,
    budget: usize,
    minted: u64,
}

impl ParticleField {
    /// An empty field with room for `budget` particles.
    ///
    /// The storage is reserved up front so a busy frame does not
    /// reallocate on the emit path.
    ///
    /// # Errors
    ///
    /// [`ArtError::OutOfMemory`] when the storage cannot be reserved.
    pub fn with_budget(budget: usize) -> Result<Self, ArtError> {
        let budget = budget.min(MAX_PARTICLES);
        let mut particles = VecDeque::new();
        particles
            .try_reserve_exact(budget)
            .map_err(|_| ArtError::OutOfMemory)?;
        Ok(Self {
            particles,
            budget,
            minted: 0,
        })
    }

    /// How many particles the field may hold.
    #[must_use]
    pub const fn budget(&self) -> usize {
        self.budget
    }

    /// The live particles, oldest first.
    ///
    /// An iterator rather than a slice, because the store is a queue: a
    /// contiguous view would cost a rotation on every read to serve a
    /// caller that walks it once.
    #[must_use]
    pub fn particles(&self) -> impl ExactSizeIterator<Item = &Particle> + '_ {
        self.particles.iter()
    }

    /// The `index`th live particle, oldest first.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&Particle> {
        self.particles.get(index)
    }

    /// How many are live.
    #[must_use]
    pub fn len(&self) -> usize {
        self.particles.len()
    }

    /// Whether the field holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.particles.is_empty()
    }

    /// Re-budget to `budget`, dropping the oldest particles over it.
    ///
    /// Called when the view changes or the pressure band moves, not on a
    /// timer.
    pub fn rebudget(&mut self, budget: usize) {
        self.budget = budget.min(MAX_PARTICLES);
        let excess = self.particles.len().saturating_sub(self.budget);
        self.particles.drain(..excess);
    }

    /// Emit a particle of `kind` at `at` with initial `velocity`.
    ///
    /// Returns whether it was admitted. A field at its budget retires its
    /// oldest to make room; a field budgeted for nothing admits nothing
    /// and says so, which is how a machine under critical pressure
    /// carries no weather.
    pub fn emit(
        &mut self,
        spawn: &Spawn,
        kind: ParticleKind,
        at: WorldPoint,
        velocity: WorldVector,
    ) -> bool {
        if self.budget == 0 {
            return false;
        }
        let variation = spawn.variation(self.minted, at);
        self.minted = self.minted.wrapping_add(1);
        let particle = Particle {
            kind,
            at,
            velocity,
            age: 0,
            variation,
        };
        if self.particles.len() >= self.budget {
            self.particles.pop_front();
        }
        self.particles.push_back(particle);
        true
    }

    /// Advance every particle one tick under `wind`, and reap the
    /// expired.
    ///
    /// Wind is one vector for the whole scene, consumed here exactly as
    /// it is by foliage, rain angle and the audio bed — there is no
    /// per-consumer copy of it.
    pub fn advance(&mut self, wind: WorldVector) {
        for particle in &mut self.particles {
            let p = params(particle.kind);
            let vx = drag_toward(particle.velocity.x, wind.x, p.drift, p.drag);
            let vy =
                drag_toward(particle.velocity.y, wind.y, p.drift, p.drag).saturating_add(p.gravity);
            particle.velocity = WorldVector { x: vx, y: vy };
            particle.at = WorldPoint {
                x: particle.at.x.saturating_add(i32::from(vx)),
                y: particle.at.y.saturating_add(i32::from(vy)),
            };
            particle.age = particle.age.saturating_add(1);
        }
        self.particles.retain(|p| !p.expired());
    }

    /// Drop every particle.
    pub fn clear(&mut self) {
        self.particles.clear();
    }
}

/// One tick of drag toward the wind: the particle keeps `drag`/255 of its
/// own velocity and is pulled `drift`/255 of the way to the wind's.
fn drag_toward(velocity: i16, wind: i16, drift: u8, drag: u8) -> i16 {
    let kept = i32::from(velocity) * i32::from(drag) / 255;
    let pull = (i32::from(wind) - kept) * i32::from(drift) / 255;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the sum is clamped into i16 before the cast"
    )]
    {
        (kept + pull).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
    }
}

/// Per-particle spawn variation, keyed on the realm.
///
/// A field of identical flakes reads as a screen effect rather than as
/// weather, and a variation drawn from a running counter alone would make
/// two clients of one realm disagree about a storm they are both
/// watching.
#[derive(Copy, Clone, Debug)]
pub struct Spawn {
    field: Tiled,
}

impl Spawn {
    /// The spawn variation for a realm.
    #[must_use]
    pub const fn new(realm_seed: u64) -> Self {
        Self {
            field: Tiled::unbounded(realm_seed),
        }
    }

    /// The variation for the `ordinal`th particle emitted at `at`.
    fn variation(&self, ordinal: u64, at: WorldPoint) -> u8 {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the ordinal only has to decorrelate two particles at one point"
        )]
        let salt = ordinal as i32;
        noise::to_byte(
            self.field
                .corner(Field::Spawn, at.x.wrapping_add(salt), at.y ^ salt),
        )
    }
}

#[cfg(test)]
mod tests;
