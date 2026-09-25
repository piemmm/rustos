//! What a frame is allowed to cost, and what happens when it costs more.
//!
//! "Playable" is not a budget and a renderer without one cannot be
//! reviewed, so the passes each have an allocation at the baseline —
//! 1280×720 at 60 Hz on a four-core machine — and a frame that overruns
//! sheds a notch of the degradation ladder rather than a frame.
//!
//! The headroom is *derived* from the frame and the named passes rather
//! than stated beside them: two numbers that must add up are one number
//! and a subtraction.
//!
//! # Why it takes several frames to move
//!
//! One slow frame is a scheduling hiccup, not a machine that cannot keep
//! up, and a renderer that shed on it would visibly flicker between
//! quality levels on an otherwise fine machine. So the governor needs a
//! run of overruns to shed and a longer run of comfortable frames to
//! restore, and the restore threshold sits well below the shed one so the
//! two cannot chase each other.

use crate::quality::Ladder;

/// Nanoseconds in a second.
const NS_PER_SEC: u64 = 1_000_000_000;

/// The refresh the budget is stated at.
pub const BASELINE_HZ: u64 = 60;

/// The whole frame's budget, in nanoseconds.
pub const FRAME_NS: u64 = NS_PER_SEC / BASELINE_HZ;

/// The width the budget is stated at.
pub const BASELINE_WIDTH: u32 = 1280;

/// The height the budget is stated at.
pub const BASELINE_HEIGHT: u32 = 720;

/// One measured stage of a frame.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Pass {
    /// Material blend and detail: the ground itself.
    Terrain,
    /// Light, fog and atmosphere composite over the ground.
    Light,
    /// Ground scenery, entities and figures, each shaded by its own
    /// surfaces and veiled by the air at its feet.
    Scenery,
    /// Particles and weather.
    Particles,
    /// Overlays the player reads rather than plays in.
    Ui,
}

impl Pass {
    /// Every pass, in the order a frame runs them.
    pub const ALL: [Self; 5] = [
        Self::Terrain,
        Self::Light,
        Self::Scenery,
        Self::Particles,
        Self::Ui,
    ];

    /// What this pass is allowed to cost at the baseline, in nanoseconds.
    #[must_use]
    pub const fn budget_ns(self) -> u64 {
        match self {
            Self::Terrain => 5_000_000,
            Self::Scenery => 3_500_000,
            Self::Particles | Self::Light => 2_000_000,
            Self::Ui => 1_000_000,
        }
    }

    /// Its index in a [`FrameTimes`].
    const fn slot(self) -> usize {
        self as usize
    }
}

/// What is left of a frame once every pass has had its allocation:
/// present, input and jitter.
#[must_use]
pub const fn headroom_ns() -> u64 {
    let mut spent = 0;
    let mut i = 0;
    while i < Pass::ALL.len() {
        spent += Pass::ALL[i].budget_ns();
        i += 1;
    }
    FRAME_NS.saturating_sub(spent)
}

/// What one frame actually cost, per pass.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct FrameTimes {
    spent: [u64; Pass::ALL.len()],
}

impl FrameTimes {
    /// Nothing measured yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            spent: [0; Pass::ALL.len()],
        }
    }

    /// Record what a pass cost.
    pub fn record(&mut self, pass: Pass, ns: u64) {
        self.spent[pass.slot()] = ns;
    }

    /// What a pass cost.
    #[must_use]
    pub const fn spent(&self, pass: Pass) -> u64 {
        self.spent[pass.slot()]
    }

    /// What every pass cost together.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.spent.iter().copied().fold(0, u64::saturating_add)
    }

    /// Whether the frame fitted, leaving the headroom the present needs.
    #[must_use]
    pub fn within_budget(&self) -> bool {
        self.total() <= FRAME_NS.saturating_sub(headroom_ns())
    }

    /// Every pass that cost more than its own allocation.
    pub fn overruns(&self) -> impl Iterator<Item = (Pass, u64)> + '_ {
        Pass::ALL
            .into_iter()
            .filter(move |p| self.spent(*p) > p.budget_ns())
            .map(move |p| (p, self.spent(p)))
    }
}

/// Consecutive overrunning frames before a notch is shed.
const SHED_AFTER: u8 = 3;

/// Consecutive comfortable frames before a notch is restored.
///
/// Longer than the shed run, so a machine that is only just fast enough
/// settles rather than oscillating across the boundary.
const RESTORE_AFTER: u8 = 60;

/// How much of the drawing budget a frame must leave unused before a
/// notch is given back, in parts per hundred.
///
/// Restoring at the same threshold it shed at would put the renderer back
/// exactly where it overran.
const RESTORE_HEADROOM_PERCENT: u64 = 70;

/// Turns the degradation ladder from what frames actually cost, no deeper
/// than the floor the view allows.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Governor {
    ladder: Ladder,
    floor: Ladder,
    over: u8,
    under: u8,
    floored: bool,
}

impl Default for Governor {
    fn default() -> Self {
        Self::new()
    }
}

impl Governor {
    /// A governor at full quality, free to shed the whole ladder until it is
    /// told a floor.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            ladder: Ladder::FULL,
            floor: Ladder::new(Ladder::MAX_STEP),
            over: 0,
            under: 0,
            floored: false,
        }
    }

    /// Where the ladder currently stands.
    #[must_use]
    pub const fn ladder(&self) -> Ladder {
        self.ladder
    }

    /// Whether frames went on overrunning with the ladder at its floor, and
    /// nothing has moved since: the frame rate is what is giving way,
    /// because the next notch would shed a detail the player needs to read.
    #[must_use]
    pub const fn floored(&self) -> bool {
        self.floored
    }

    /// Go no deeper than `floor` — the deepest the window's size and the
    /// zoom let the ladder go without shedding a detail the player needs —
    /// returning whether the ladder moved.
    ///
    /// Called before a frame is drawn, so no frame is drawn past its own
    /// floor. A floor that has come up past the ladder takes it back at once
    /// rather than after a comfortable run, since what it is shedding is now
    /// something the player needs to read.
    pub fn hold(&mut self, floor: Ladder) -> bool {
        self.floor = floor;
        if self.ladder > floor {
            self.moved_to(floor);
            return true;
        }
        if self.ladder < floor {
            self.floored = false;
        }
        false
    }

    /// Account for a finished frame, returning whether the ladder moved.
    ///
    /// Frame rate is never what gives way while there is a notch to shed:
    /// this only ever turns a ladder notch. At the floor, and at the
    /// ladder's end, nothing more is shed; the frame rate gives way, and
    /// [`Self::floored`] says so.
    pub fn observe(&mut self, times: &FrameTimes) -> bool {
        let drawing = FRAME_NS.saturating_sub(headroom_ns());
        let total = times.total();
        if total > drawing {
            self.under = 0;
            self.over = self.over.saturating_add(1);
            if self.over >= SHED_AFTER {
                self.over = 0;
                match self.ladder.shed().filter(|next| *next <= self.floor) {
                    Some(next) => {
                        self.moved_to(next);
                        return true;
                    }
                    None => self.floored = true,
                }
            }
            return false;
        }
        self.over = 0;
        if total * 100 <= drawing.saturating_mul(RESTORE_HEADROOM_PERCENT) {
            self.under = self.under.saturating_add(1);
            if self.under >= RESTORE_AFTER {
                if let Some(next) = self.ladder.restore() {
                    self.moved_to(next);
                    return true;
                }
                self.under = 0;
            }
        } else {
            self.under = 0;
        }
        false
    }

    fn moved_to(&mut self, ladder: Ladder) {
        self.ladder = ladder;
        self.over = 0;
        self.under = 0;
        self.floored = false;
    }
}

#[cfg(test)]
#[path = "budget_tests.rs"]
mod tests;
