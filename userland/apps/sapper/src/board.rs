//! The board: the rules of the game, and the ordered wave every action
//! produces for the animation to be timed from.
//!
//! Nothing here draws, reads a clock, or performs I/O, so every rule below is a
//! host test. An action returns a [`Move`]: the cells whose drawn state
//! changed, each tagged with the breadth-first *ring* it was reached on, and
//! what the action concluded. The ring is what turns a flood fill into a
//! cascade rippling outward from the click rather than a region that blinks
//! into existence, and the same list is the damage set the repaint is scoped
//! to — one answer, not two.
//!
//! # Mines are laid on the first click, never before
//!
//! A board starts empty and lays its mines when the first cell is revealed,
//! excluding that cell **and its eight neighbours**. The opening move therefore
//! always opens a region rather than a bare number, and can never lose.
//!
//! Boards are otherwise uniformly random: proving one solvable without a guess
//! would need a solver in the generator, which is a deliberate non-goal.

use alloc::vec;
use alloc::vec::Vec;

use tairix_rng::RandU64;

/// The fewest cells a board may have on a side.
///
/// Below four there is no room for a mine outside the opening move's safe
/// region, so no valid mine count would exist.
pub const MIN_SIDE: u16 = 4;

/// The most cells a board may have on a side.
///
/// A fixed validation bound on a custom size that may arrive from a settings
/// document the user can hand-edit, not a capacity: a board larger than this is
/// unplayable on any screen the desktop drives, so a larger request is refused
/// rather than sized up to.
pub const MAX_SIDE: u16 = 64;

/// Cells the opening move is guaranteed to open: the clicked cell and the eight
/// around it.
const SAFE_REGION: u32 = 9;

/// A cell's position on the board.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Coord {
    /// Zero-based column, left to right.
    pub col: u16,
    /// Zero-based row, top to bottom.
    pub row: u16,
}

impl Coord {
    /// A coordinate at `col`, `row`.
    #[must_use]
    pub const fn new(col: u16, row: u16) -> Self {
        Self { col, row }
    }
}

/// A validated board size and mine count.
///
/// Private fields: the invariants — sides within bounds, at least one mine, and
/// room for the opening move's safe region — hold for every value that exists.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Dimensions {
    cols: u16,
    rows: u16,
    mines: u32,
}

impl Dimensions {
    /// A size and mine count, or `None` for one the rules cannot satisfy.
    ///
    /// Refused: a side outside [`MIN_SIDE`]`..=`[`MAX_SIDE`], no mines at all,
    /// or more mines than fit outside the opening move's safe region — which
    /// would make a guaranteed-safe first click impossible.
    #[must_use]
    pub const fn new(cols: u16, rows: u16, mines: u32) -> Option<Self> {
        if cols < MIN_SIDE || cols > MAX_SIDE || rows < MIN_SIDE || rows > MAX_SIDE {
            return None;
        }
        if mines == 0 || mines > Self::max_mines(cols, rows) {
            return None;
        }
        Some(Self { cols, rows, mines })
    }

    /// The most mines a `cols`×`rows` board may hold: every cell outside the
    /// opening move's safe region.
    #[must_use]
    pub const fn max_mines(cols: u16, rows: u16) -> u32 {
        (cols as u32 * rows as u32).saturating_sub(SAFE_REGION)
    }

    /// Columns.
    #[must_use]
    pub const fn cols(self) -> u16 {
        self.cols
    }

    /// Rows.
    #[must_use]
    pub const fn rows(self) -> u16 {
        self.rows
    }

    /// Mines.
    #[must_use]
    pub const fn mines(self) -> u32 {
        self.mines
    }

    /// Total cells.
    #[must_use]
    pub const fn cells(self) -> u32 {
        self.cols as u32 * self.rows as u32
    }
}

/// The 9×9, 10-mine board.
///
/// The presets are spelled as literals here, inside the module that owns the
/// invariant, so no fallible construction sits on a path that cannot fail.
/// `presets_are_valid` proves each one satisfies [`Dimensions::new`].
const BEGINNER: Dimensions = Dimensions {
    cols: 9,
    rows: 9,
    mines: 10,
};
/// The 16×16, 40-mine board.
const INTERMEDIATE: Dimensions = Dimensions {
    cols: 16,
    rows: 16,
    mines: 40,
};
/// The 30×16, 99-mine board.
const EXPERT: Dimensions = Dimensions {
    cols: 30,
    rows: 16,
    mines: 99,
};

/// One of the standard board sizes, or a validated size of the player's own.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Difficulty {
    /// 9×9 with 10 mines.
    Beginner,
    /// 16×16 with 40 mines.
    Intermediate,
    /// 30×16 with 99 mines.
    Expert,
    /// A size the player chose.
    Custom(Dimensions),
}

impl Difficulty {
    /// The three standard sizes, in increasing order.
    pub const PRESETS: [Self; 3] = [Self::Beginner, Self::Intermediate, Self::Expert];

    /// This difficulty's board size.
    #[must_use]
    pub const fn dimensions(self) -> Dimensions {
        match self {
            Self::Beginner => BEGINNER,
            Self::Intermediate => INTERMEDIATE,
            Self::Expert => EXPERT,
            Self::Custom(dims) => dims,
        }
    }

    /// The locale-neutral identifier the best-times store spells this
    /// difficulty with.
    ///
    /// A custom size keeps no best time — no two custom boards are the same
    /// game — so only the presets have one.
    #[must_use]
    pub const fn preset_id(self) -> Option<&'static str> {
        match self {
            Self::Beginner => Some("beginner"),
            Self::Intermediate => Some("intermediate"),
            Self::Expert => Some("expert"),
            Self::Custom(_) => None,
        }
    }

    /// The preset a stored identifier names; `None` for anything else.
    #[must_use]
    pub fn from_preset_id(id: &str) -> Option<Self> {
        Self::PRESETS
            .into_iter()
            .find(|preset| preset.preset_id() == Some(id))
    }

    /// The name shown in the menu and the best-times list.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Beginner => "Beginner",
            Self::Intermediate => "Intermediate",
            Self::Expert => "Expert",
            Self::Custom(_) => "Custom",
        }
    }
}

/// What a cell shows.
///
/// The endgame states are part of this vocabulary rather than a second table
/// beside it, so a painter that handles every variant has drawn every reachable
/// board and no cell can be in two states at once.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Cover {
    /// Untouched.
    Covered,
    /// The player marked it as a mine.
    Flagged,
    /// The player marked it uncertain.
    Questioned,
    /// Revealed, showing its adjacent-mine count.
    Open,
    /// A mine, shown because the game was lost.
    Exposed {
        /// Whether this is the mine the player actually struck. The rest were
        /// merely uncovered when the game ended.
        struck: bool,
    },
    /// A flag on a cell that held no mine, shown because the game was lost.
    Misflagged,
}

/// One cell.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Cell {
    mine: bool,
    /// Mines in the eight surrounding cells, `0..=8`.
    adjacent: u8,
    cover: Cover,
}

/// How far a game has got.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Phase {
    /// No cell revealed yet, so no mine is laid and the clock has not started.
    Ready,
    /// Under way.
    Playing,
    /// Every cell that is not a mine is open.
    Won,
    /// A mine was struck.
    Lost,
}

impl Phase {
    /// Whether the game has finished, either way.
    #[must_use]
    pub const fn is_over(self) -> bool {
        matches!(self, Self::Won | Self::Lost)
    }
}

/// One cell an action changed, and the breadth-first ring it was reached on.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Step {
    /// The cell.
    pub at: Coord,
    /// Rings from the acted cell: `0` is the cell itself, `1` its neighbours,
    /// and so on. The animation staggers a cell's start by this.
    pub ring: u16,
}

/// What an action concluded.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum Outcome {
    /// The action changed nothing.
    #[default]
    Nothing,
    /// Cells were revealed.
    Opened,
    /// A mark was placed, moved, or cleared.
    Marked,
    /// A mine was struck; the game is lost.
    Detonated,
    /// The last safe cell was revealed; the game is won.
    Won,
}

/// What one action did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Move {
    /// The cells whose drawn state changed, in ring order from the acted cell.
    /// Also the repaint's damage set.
    pub steps: Vec<Step>,
    /// What the action concluded.
    pub outcome: Outcome,
}

impl Move {
    /// Whether anything changed.
    #[must_use]
    pub fn is_nothing(&self) -> bool {
        self.outcome == Outcome::Nothing && self.steps.is_empty()
    }
}

/// A game in progress.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Board {
    dims: Dimensions,
    cells: Vec<Cell>,
    phase: Phase,
    /// Whether the mines are laid, which happens on the first reveal.
    laid: bool,
    /// Open cells that are not mines — the win condition's counter.
    opened: u32,
    /// Flags placed, so the remaining-mine reading costs no scan.
    flags: u32,
    /// Whether the third mark in the cycle — the question mark — is offered.
    questions: bool,
}

impl Board {
    /// An untouched board of `dims`, with no mine laid yet.
    #[must_use]
    pub fn new(dims: Dimensions, questions: bool) -> Self {
        let cell = Cell {
            mine: false,
            adjacent: 0,
            cover: Cover::Covered,
        };
        Self {
            dims,
            cells: vec![cell; dims.cells() as usize],
            phase: Phase::Ready,
            laid: false,
            opened: 0,
            flags: 0,
            questions,
        }
    }

    /// The board's size and mine count.
    #[must_use]
    pub const fn dimensions(&self) -> Dimensions {
        self.dims
    }

    /// How far the game has got.
    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// Whether the question mark is in the mark cycle.
    #[must_use]
    pub const fn questions(&self) -> bool {
        self.questions
    }

    /// Offer, or stop offering, the question mark.
    ///
    /// A question mark already placed is left alone: it is the player's own
    /// note, and clearing it would lose what they entered. The next cycle
    /// through that cell simply skips the state.
    pub const fn set_questions(&mut self, questions: bool) {
        self.questions = questions;
    }

    /// Mines less flags placed — what the counter shows.
    ///
    /// Signed, because a player may place more flags than there are mines and
    /// the counter should say so rather than clamp at zero and lie.
    #[must_use]
    pub const fn remaining(&self) -> i64 {
        self.dims.mines as i64 - self.flags as i64
    }

    /// Whether `at` is on the board.
    #[must_use]
    pub const fn contains(&self, at: Coord) -> bool {
        at.col < self.dims.cols && at.row < self.dims.rows
    }

    /// What the cell at `at` shows, or `None` when it is off the board.
    #[must_use]
    pub fn cover(&self, at: Coord) -> Option<Cover> {
        self.cells.get(self.index(at)?).map(|cell| cell.cover)
    }

    /// The mine count around `at`, or `None` when it is off the board.
    #[must_use]
    pub fn adjacent(&self, at: Coord) -> Option<u8> {
        self.cells.get(self.index(at)?).map(|cell| cell.adjacent)
    }

    /// Whether `at` holds a mine, or `None` when it is off the board.
    ///
    /// The painter never asks — a cell shows what its [`Cover`] says — but the
    /// rules and their tests do.
    #[must_use]
    pub fn is_mine(&self, at: Coord) -> Option<bool> {
        self.cells.get(self.index(at)?).map(|cell| cell.mine)
    }

    /// Every cell, in row-major order, as `(coord, cover, adjacent)`.
    pub fn iter(&self) -> impl Iterator<Item = (Coord, Cover, u8)> + '_ {
        (0..self.cells.len()).filter_map(move |index| {
            let at = self.coord(index)?;
            let cell = self.cells.get(index)?;
            Some((at, cell.cover, cell.adjacent))
        })
    }

    fn index(&self, at: Coord) -> Option<usize> {
        if !self.contains(at) {
            return None;
        }
        Some(usize::from(at.row) * usize::from(self.dims.cols) + usize::from(at.col))
    }

    fn coord(&self, index: usize) -> Option<Coord> {
        let cols = usize::from(self.dims.cols);
        let col = u16::try_from(index.checked_rem(cols)?).ok()?;
        let row = u16::try_from(index.checked_div(cols)?).ok()?;
        let at = Coord::new(col, row);
        self.contains(at).then_some(at)
    }

    /// The in-bounds cells touching `at`, `at` excluded.
    fn neighbours(&self, at: Coord) -> impl Iterator<Item = Coord> {
        neighbours_of(at, self.dims.cols, self.dims.rows)
    }

    /// Reveal `at`, laying the mines first if this is the opening move.
    ///
    /// A flagged cell is protected: revealing it would defeat the point of
    /// having marked it. A question mark is not — it records doubt, not a
    /// decision.
    pub fn reveal(&mut self, at: Coord, rng: &mut dyn RandU64) -> Move {
        if self.phase.is_over() || !self.is_unopened(at) {
            return Move::default();
        }
        self.lay_if_needed(at, rng);
        if self.is_mine(at) == Some(true) {
            return self.detonate(at);
        }
        let mut opened = self.flood(&[at]);
        self.settle(&mut opened);
        opened
    }

    /// Reveal every unflagged neighbour of an open `at` whose flag count
    /// already matches its number — the classic chord.
    ///
    /// Nothing happens unless the flags match exactly: a chord asserts the
    /// marking around that cell is complete, and acting on an incomplete
    /// assertion would reveal cells the player never chose.
    ///
    /// A chord that uncovers several mines at once names the first in
    /// neighbour order as the struck one, so the detonation has a single,
    /// deterministic origin to spread from.
    pub fn chord(&mut self, at: Coord) -> Move {
        if self.phase.is_over() || self.cover(at) != Some(Cover::Open) {
            return Move::default();
        }
        let Some(adjacent) = self.adjacent(at) else {
            return Move::default();
        };
        if adjacent == 0 || u32::from(adjacent) != self.count_flags(at) {
            return Move::default();
        }
        let seeds: Vec<Coord> = self
            .neighbours(at)
            .filter(|&n| self.is_unopened(n))
            .collect();
        if seeds.is_empty() {
            return Move::default();
        }
        if let Some(&struck) = seeds.iter().find(|&&n| self.is_mine(n) == Some(true)) {
            return self.detonate(struck);
        }
        let mut opened = self.flood(&seeds);
        self.settle(&mut opened);
        opened
    }

    /// Flag every covered neighbour of an open `at` when they are exactly as
    /// many as its number — the mirror of [`chord`](Self::chord).
    ///
    /// The deduction a player makes constantly and then executes one click at a
    /// time. The classic game has no such command.
    pub fn flag_chord(&mut self, at: Coord) -> Move {
        if self.phase.is_over() || self.cover(at) != Some(Cover::Open) {
            return Move::default();
        }
        let Some(adjacent) = self.adjacent(at) else {
            return Move::default();
        };
        let unopened: Vec<Coord> = self
            .neighbours(at)
            .filter(|&n| self.cover(n) != Some(Cover::Open))
            .collect();
        if adjacent == 0 || usize::from(adjacent) != unopened.len() {
            return Move::default();
        }
        let mut steps = Vec::with_capacity(unopened.len());
        for cell in unopened {
            if self.is_unopened(cell) {
                self.set_cover(cell, Cover::Flagged);
                self.flags += 1;
                steps.push(Step { at: cell, ring: 1 });
            }
        }
        if steps.is_empty() {
            return Move::default();
        }
        Move {
            steps,
            outcome: Outcome::Marked,
        }
    }

    /// Step `at` through the mark cycle: covered → flagged → (questioned →)
    /// covered.
    pub fn toggle_mark(&mut self, at: Coord) -> Move {
        if self.phase.is_over() {
            return Move::default();
        }
        let was = self.cover(at);
        let next = match was {
            Some(Cover::Covered) => Cover::Flagged,
            Some(Cover::Flagged) if self.questions => Cover::Questioned,
            Some(Cover::Flagged | Cover::Questioned) => Cover::Covered,
            _ => return Move::default(),
        };
        if next == Cover::Flagged {
            self.flags += 1;
        } else if was == Some(Cover::Flagged) {
            self.flags = self.flags.saturating_sub(1);
        }
        self.set_cover(at, next);
        Move {
            steps: vec![Step { at, ring: 0 }],
            outcome: Outcome::Marked,
        }
    }

    /// Whether `at` is a cell a reveal may open: covered, or merely questioned.
    fn is_unopened(&self, at: Coord) -> bool {
        matches!(self.cover(at), Some(Cover::Covered | Cover::Questioned))
    }

    /// Flags among the cells touching `at`.
    fn count_flags(&self, at: Coord) -> u32 {
        u32::try_from(
            self.neighbours(at)
                .filter(|&n| self.cover(n) == Some(Cover::Flagged))
                .count(),
        )
        .unwrap_or(u32::MAX)
    }

    fn set_cover(&mut self, at: Coord, cover: Cover) {
        if let Some(cell) = self.index(at).and_then(|i| self.cells.get_mut(i)) {
            cell.cover = cover;
        }
    }

    fn lay_if_needed(&mut self, first: Coord, rng: &mut dyn RandU64) {
        if self.laid {
            return;
        }
        self.lay(first, rng);
        self.phase = Phase::Playing;
    }

    /// Lay the mines, keeping `first` and its neighbours clear, then count each
    /// cell's neighbours.
    ///
    /// A partial Fisher–Yates over the eligible cells, drawing each index from
    /// the shared unbiased bounded draw — so every legal layout is equally
    /// likely, which a modulo of a raw word would not give.
    fn lay(&mut self, first: Coord, rng: &mut dyn RandU64) {
        let mut safe = Vec::with_capacity(SAFE_REGION as usize);
        safe.push(first);
        safe.extend(self.neighbours(first));

        let mut pool: Vec<usize> = (0..self.cells.len())
            .filter(|&i| self.coord(i).is_some_and(|at| !safe.contains(&at)))
            .collect();
        let wanted = (self.dims.mines as usize).min(pool.len());
        for k in 0..wanted {
            let span = pool.len() - k;
            // `next_below` answers below its bound, so the clamp is unreachable
            // through the generators in this workspace; it is what keeps a
            // hypothetical bad one from indexing off the end.
            let offset = usize::try_from(rng.next_below(span as u64))
                .unwrap_or(0)
                .min(span - 1);
            pool.swap(k, k + offset);
            if let Some(cell) = pool.get(k).and_then(|&i| self.cells.get_mut(i)) {
                cell.mine = true;
            }
        }

        for index in 0..self.cells.len() {
            let Some(at) = self.coord(index) else {
                continue;
            };
            let count = self
                .neighbours(at)
                .filter(|&n| self.is_mine(n) == Some(true))
                .count();
            if let Some(cell) = self.cells.get_mut(index) {
                cell.adjacent = u8::try_from(count).unwrap_or(u8::MAX);
            }
        }
        self.laid = true;
    }

    /// Open `seeds` and, transitively, everything a zero-count cell touches,
    /// breadth-first so each cell carries the ring it was reached on.
    fn flood(&mut self, seeds: &[Coord]) -> Move {
        let mut steps = Vec::new();
        let mut frontier: Vec<Coord> = Vec::new();
        for &seed in seeds {
            if self.is_unopened(seed) {
                self.open(seed);
                steps.push(Step { at: seed, ring: 0 });
                frontier.push(seed);
            }
        }
        let (cols, rows) = (self.dims.cols, self.dims.rows);
        let mut ring = 0_u16;
        let mut next: Vec<Coord> = Vec::new();
        while !frontier.is_empty() {
            ring = ring.saturating_add(1);
            for &cell in &frontier {
                if self.adjacent(cell) != Some(0) {
                    continue;
                }
                for n in neighbours_of(cell, cols, rows) {
                    if self.is_unopened(n) {
                        self.open(n);
                        steps.push(Step { at: n, ring });
                        next.push(n);
                    }
                }
            }
            core::mem::swap(&mut frontier, &mut next);
            next.clear();
        }
        let outcome = if steps.is_empty() {
            Outcome::Nothing
        } else {
            Outcome::Opened
        };
        Move { steps, outcome }
    }

    /// Open one cell, keeping the win counter true.
    ///
    /// Only ever called for a cell [`is_unopened`](Self::is_unopened) admits, so
    /// a flag is never overwritten and the flag count cannot drift.
    fn open(&mut self, at: Coord) {
        self.set_cover(at, Cover::Open);
        self.opened += 1;
    }

    /// Declare the win when the last safe cell has just been opened, planting a
    /// flag on every mine still covered and appending those cells to `opened`
    /// so the caller repaints them.
    fn settle(&mut self, opened: &mut Move) {
        if self.opened < self.dims.cells().saturating_sub(self.dims.mines) {
            return;
        }
        self.phase = Phase::Won;
        let ring = opened
            .steps
            .iter()
            .map(|step| step.ring)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let unflagged: Vec<Coord> = (0..self.cells.len())
            .filter(|&i| {
                self.cells
                    .get(i)
                    .is_some_and(|c| c.mine && c.cover != Cover::Flagged)
            })
            .filter_map(|i| self.coord(i))
            .collect();
        for at in unflagged {
            self.set_cover(at, Cover::Flagged);
            self.flags += 1;
            opened.steps.push(Step { at, ring });
        }
        opened.outcome = Outcome::Won;
    }

    /// End the game on the mine at `struck`: expose every mine outward from it
    /// and mark every flag that was wrong.
    ///
    /// The ring is the Chebyshev distance from the struck mine, so the chain
    /// detonates outward from where the player actually clicked.
    fn detonate(&mut self, struck: Coord) -> Move {
        self.phase = Phase::Lost;
        self.set_cover(struck, Cover::Exposed { struck: true });
        let mut rest: Vec<Step> = Vec::new();
        for index in 0..self.cells.len() {
            let Some(at) = self.coord(index) else {
                continue;
            };
            let Some(&cell) = self.cells.get(index) else {
                continue;
            };
            if at == struck {
                continue;
            }
            let exposed = match (cell.mine, cell.cover) {
                (true, Cover::Covered | Cover::Questioned) => Cover::Exposed { struck: false },
                (false, Cover::Flagged) => Cover::Misflagged,
                _ => continue,
            };
            self.set_cover(at, exposed);
            rest.push(Step {
                at,
                ring: chebyshev(struck, at),
            });
        }
        rest.sort_unstable_by_key(|step| (step.ring, step.at));
        let mut steps = vec![Step {
            at: struck,
            ring: 0,
        }];
        steps.extend(rest);
        Move {
            steps,
            outcome: Outcome::Detonated,
        }
    }
}

/// The in-bounds cells touching `at` on a `cols`×`rows` board, `at` excluded.
///
/// Free of the board so a flood may open a cell while walking its neighbours.
fn neighbours_of(at: Coord, cols: u16, rows: u16) -> impl Iterator<Item = Coord> {
    [
        (-1_i32, -1_i32),
        (0, -1),
        (1, -1),
        (-1, 0),
        (1, 0),
        (-1, 1),
        (0, 1),
        (1, 1),
    ]
    .into_iter()
    .filter_map(move |(dc, dr)| {
        let col = u16::try_from(i32::from(at.col) + dc).ok()?;
        let row = u16::try_from(i32::from(at.row) + dr).ok()?;
        (col < cols && row < rows).then_some(Coord::new(col, row))
    })
}

/// Rings between two cells: the larger of the column and row separations, which
/// is how far a wave spreading in all eight directions has to travel.
fn chebyshev(from: Coord, to: Coord) -> u16 {
    from.col.abs_diff(to.col).max(from.row.abs_diff(to.row))
}

#[cfg(test)]
#[path = "board_tests.rs"]
mod tests;
