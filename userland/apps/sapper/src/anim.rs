//! The animation model: what each cell is doing right now, and when the next
//! frame is due.
//!
//! A board action hands its [`Move`](crate::board::Move) here as a **wave** —
//! the cells it changed, each starting a ring's worth of time after the one
//! before, so a cascade ripples outward from the click and a detonation chains
//! outward from the struck mine. The painter asks [`Motion::cell`] what a cell
//! is doing; nothing here draws.
//!
//! # It is never a loop, and it never runs when nothing changed
//!
//! Every wave is finite and begins at a state change. [`Motion::deadline_ns`]
//! answers `None` the moment the last one ends, and the event loop then parks
//! with no deadline at all — so an idle game costs no wake, no timer, and no
//! CPU. There is no decorative idle animation anywhere in this app.
//!
//! Reduced motion is honoured by not starting a wave at all: the board state is
//! applied before the wave is offered, so a suppressed wave means every cell
//! draws its finished appearance immediately. There is no second code path.

use alloc::vec::Vec;

use crate::board::{Coord, Step};

/// How long one animated frame lasts: a sixtieth of a second.
///
/// A repaint is scoped to the cells actually moving, so the cost of a frame is
/// a handful of tiles rather than the window; and a wave is short and finite,
/// so a game at rest wakes for nothing at all.
pub const FRAME_NS: u64 = 1_000_000_000 / 60;

/// Live waves kept at once, oldest dropped past it.
///
/// Each action starts one wave and every wave is under two seconds, so reaching
/// this needs several actions inside one animation. Dropping the oldest costs
/// only its remaining motion — the board state it described was applied when it
/// began — so the cells it covered simply finish at once.
const MAX_WAVES: usize = 4;

/// What a wave is animating.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WaveKind {
    /// A cover lifting off a cell the player opened.
    Reveal,
    /// A mark arriving on, or leaving, a cell.
    Mark,
    /// Mines exposing outward from the one that was struck.
    Detonate,
    /// The sweep over a finished board when the last safe cell is opened.
    Victory,
    /// An action the rules refused, shaken to say so.
    Rejected,
}

impl WaveKind {
    /// How long one cell's part of this wave lasts, in nanoseconds.
    const fn span_ns(self) -> u64 {
        match self {
            Self::Reveal => 140_000_000,
            Self::Mark => 160_000_000,
            Self::Detonate => 260_000_000,
            Self::Victory => 300_000_000,
            Self::Rejected => 240_000_000,
        }
    }

    /// How long each further ring waits before it starts, in nanoseconds.
    const fn stagger_ns(self) -> u64 {
        match self {
            Self::Reveal => 22_000_000,
            Self::Mark => 34_000_000,
            Self::Detonate => 45_000_000,
            Self::Victory => 26_000_000,
            Self::Rejected => 0,
        }
    }
}

/// What one cell is doing this frame.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CellMotion {
    /// Which wave the cell belongs to.
    pub kind: WaveKind,
    /// How far through its own span it is: `0` at the start, `255` at the end.
    /// A cell whose ring has not come round yet reads `0`.
    pub progress: u8,
}

impl CellMotion {
    /// The eased progress, decelerating into its resting state.
    #[must_use]
    pub fn eased(self) -> u8 {
        ease_out(self.progress)
    }

    /// The eased progress as a per-mille scale that overshoots its target
    /// before settling — what makes a flag plant rather than fade in.
    #[must_use]
    pub fn overshoot(self) -> u16 {
        overshoot(self.progress)
    }
}

/// One cell of a wave: where it is, and the ring that delays its start.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Member {
    /// Row-major cell index, so the lookup is a binary search.
    cell: u32,
    ring: u16,
}

/// One animated state change, spreading outward by ring.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Wave {
    kind: WaveKind,
    started_ns: u64,
    /// Sorted by `cell`, so a painter's per-cell question is a binary search.
    members: Vec<Member>,
    /// When the last ring finishes.
    ends_ns: u64,
}

impl Wave {
    /// This cell's progress, or `None` when the wave does not cover it.
    fn progress(&self, cell: u32, now_ns: u64) -> Option<u8> {
        let index = self.members.binary_search_by_key(&cell, |m| m.cell).ok()?;
        let member = self.members.get(index)?;
        let start = self
            .started_ns
            .saturating_add(self.kind.stagger_ns() * u64::from(member.ring));
        let elapsed = now_ns.saturating_sub(start);
        if now_ns < start {
            return Some(0);
        }
        let span = self.kind.span_ns();
        if elapsed >= span {
            return Some(u8::MAX);
        }
        u8::try_from(elapsed.saturating_mul(255) / span).ok()
    }
}

/// Every wave in flight over one board.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Motion {
    waves: Vec<Wave>,
    /// The board's column count, so a coordinate becomes a cell index.
    cols: u16,
    /// Whether the active theme asks for animation to be suppressed.
    reduced: bool,
}

impl Motion {
    /// A motion set for a board `cols` wide, animating unless `reduced`.
    #[must_use]
    pub const fn new(cols: u16, reduced: bool) -> Self {
        Self {
            waves: Vec::new(),
            cols,
            reduced,
        }
    }

    /// Adopt a theme's reduced-motion policy, ending everything in flight when
    /// it is turned on so no half-finished cell is left mid-animation.
    pub fn set_reduced_motion(&mut self, reduced: bool) {
        self.reduced = reduced;
        if reduced {
            self.waves.clear();
        }
    }

    /// Whether animation is suppressed.
    #[must_use]
    pub const fn reduced_motion(&self) -> bool {
        self.reduced
    }

    /// Forget every wave — a new board has nothing to finish animating.
    pub fn clear(&mut self) {
        self.waves.clear();
    }

    /// Whether nothing is animating.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.waves.is_empty()
    }

    /// Start a wave of `kind` over `steps` at `now_ns`.
    ///
    /// Under reduced motion nothing is started: the cells draw their finished
    /// state, which they already hold.
    pub fn begin(&mut self, kind: WaveKind, steps: &[Step], now_ns: u64) {
        if self.reduced || steps.is_empty() || self.cols == 0 {
            return;
        }
        let mut members: Vec<Member> = steps
            .iter()
            .map(|step| Member {
                cell: self.cell_index(step.at),
                ring: step.ring,
            })
            .collect();
        members.sort_unstable_by_key(|m| m.cell);
        members.dedup_by_key(|m| m.cell);
        let last_ring = members.iter().map(|m| m.ring).max().unwrap_or(0);
        let ends_ns = now_ns
            .saturating_add(kind.stagger_ns().saturating_mul(u64::from(last_ring)))
            .saturating_add(kind.span_ns());
        if self.waves.len() >= MAX_WAVES {
            self.waves.remove(0);
        }
        self.waves.push(Wave {
            kind,
            started_ns: now_ns,
            members,
            ends_ns,
        });
    }

    /// Drop every wave that has finished by `now_ns`, reporting whether any
    /// remains.
    pub fn advance(&mut self, now_ns: u64) -> bool {
        self.waves.retain(|wave| wave.ends_ns > now_ns);
        !self.waves.is_empty()
    }

    /// What `at` is doing, or `None` when it is at rest.
    ///
    /// The newest wave covering the cell wins: a later state change supersedes
    /// whatever the cell was still finishing.
    #[must_use]
    pub fn cell(&self, at: Coord, now_ns: u64) -> Option<CellMotion> {
        let cell = self.cell_index(at);
        self.waves.iter().rev().find_map(|wave| {
            wave.progress(cell, now_ns).map(|progress| CellMotion {
                kind: wave.kind,
                progress,
            })
        })
    }

    /// When the next frame is due, or `None` when nothing is animating.
    ///
    /// One-shot by construction: the caller passes this as its park deadline,
    /// so a board at rest parks with no deadline and takes no wake at all.
    #[must_use]
    pub fn deadline_ns(&self, now_ns: u64) -> Option<u64> {
        let last = self.waves.iter().map(|wave| wave.ends_ns).max()?;
        Some(now_ns.saturating_add(FRAME_NS).min(last))
    }

    /// Every cell any live wave covers, for the repaint's damage set.
    pub fn animating(&self) -> impl Iterator<Item = Coord> + '_ {
        self.waves
            .iter()
            .flat_map(|wave| wave.members.iter())
            .filter_map(|member| self.cell_coord(member.cell))
    }

    fn cell_index(&self, at: Coord) -> u32 {
        u32::from(at.row) * u32::from(self.cols) + u32::from(at.col)
    }

    fn cell_coord(&self, cell: u32) -> Option<Coord> {
        if self.cols == 0 {
            return None;
        }
        let cols = u32::from(self.cols);
        Some(Coord::new(
            u16::try_from(cell % cols).ok()?,
            u16::try_from(cell / cols).ok()?,
        ))
    }
}

/// Decelerate into the resting state: fast at first, settling at the end.
fn ease_out(t: u8) -> u8 {
    let inverse = u32::from(255 - t);
    let cubed = inverse * inverse * inverse / (255 * 255);
    255 - u8::try_from(cubed).unwrap_or(255)
}

/// A per-mille scale that passes its target and settles back — the "back out"
/// curve, which is what makes a mark land rather than fade in.
///
/// `0` at the start and `1000` at the end, rising above `1000` in between.
fn overshoot(t: u8) -> u16 {
    // The classic back-out constants, carried in per-mille so the whole curve
    // is integer arithmetic.
    const TENSION: i64 = 1702;
    const CUBIC: i64 = 2702;
    let progress = i64::from(t) * 1000 / 255;
    let d = progress - 1000;
    let value = 1000 + (CUBIC * d * d * d) / 1_000_000_000 + (TENSION * d * d) / 1_000_000;
    u16::try_from(value.max(0)).unwrap_or(u16::MAX)
}

#[cfg(test)]
#[path = "anim_tests.rs"]
mod tests;
