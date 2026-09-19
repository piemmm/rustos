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
    /// Decals, ground scenery, entities and figures.
    Scenery,
    /// Particles and weather.
    Particles,
    /// Light, fog and atmosphere composite.
    Light,
    /// Overlays the player reads rather than plays in.
    Ui,
}

impl Pass {
    /// Every pass, in the order a frame runs them.
    pub const ALL: [Self; 5] = [
        Self::Terrain,
        Self::Scenery,
        Self::Particles,
        Self::Light,
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

/// Turns the degradation ladder from what frames actually cost.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Governor {
    ladder: Ladder,
    over: u8,
    under: u8,
}

impl Default for Governor {
    fn default() -> Self {
        Self::new()
    }
}

impl Governor {
    /// A governor at full quality.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            ladder: Ladder::FULL,
            over: 0,
            under: 0,
        }
    }

    /// Where the ladder currently stands.
    #[must_use]
    pub const fn ladder(&self) -> Ladder {
        self.ladder
    }

    /// Account for a finished frame, returning whether the ladder moved.
    ///
    /// Frame rate is never what gives way: this only ever turns a ladder
    /// notch, and when the ladder is spent it leaves the renderer where
    /// it is rather than dropping frames on purpose.
    pub fn observe(&mut self, times: &FrameTimes) -> bool {
        let drawing = FRAME_NS.saturating_sub(headroom_ns());
        let total = times.total();
        if total > drawing {
            self.under = 0;
            self.over = self.over.saturating_add(1);
            if self.over >= SHED_AFTER {
                self.over = 0;
                if let Some(next) = self.ladder.shed() {
                    self.ladder = next;
                    return true;
                }
            }
            return false;
        }
        self.over = 0;
        if total * 100 <= drawing.saturating_mul(RESTORE_HEADROOM_PERCENT) {
            self.under = self.under.saturating_add(1);
            if self.under >= RESTORE_AFTER {
                self.under = 0;
                if let Some(next) = self.ladder.restore() {
                    self.ladder = next;
                    return true;
                }
            }
        } else {
            self.under = 0;
        }
        false
    }
}

#[cfg(test)]
#[path = "budget_tests.rs"]
mod tests;
