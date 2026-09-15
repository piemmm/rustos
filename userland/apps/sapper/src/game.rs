//! The composed game: the model the `Run` binary drives.
//!
//! It owns the board, the layout it is drawn in, the animation in flight, the
//! clock, and the best times — and turns one delivered input event into a board
//! action, a wave, and a reported damage rectangle.
//!
//! Nothing here performs I/O. The clock is a reading the caller passes in and
//! the best-times *write* is handed back for the caller's worker to do, because
//! this runs on the loop that owes the window a frame.
//!
//! # What a reported repaint covers
//!
//! Every path reports only the cells it changed, plus the header when a reading
//! on it moved. A cascade across half an Expert board therefore repaints those
//! cells and nothing else, and a clock tick repaints one readout.

use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_raster::Surface;
use tairix_rng::RandU64;
use tairix_theme::Theme;

use crate::anim::{Motion, WaveKind};
use crate::board::{Board, Coord, Cover, Difficulty, Move, Outcome, Phase, Step};
use crate::layout::Layout;
use crate::paint::{self, Focus, Skin};
use crate::scores::{BestTimes, MAX_TIME_SECS};

/// A second, in nanoseconds — the clock's own tick.
const SECOND_NS: u64 = 1_000_000_000;

/// What one delivered event concluded, for the caller's loop.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Reaction {
    /// Whether the view changed and must be re-presented.
    pub changed: bool,
    /// Whether a new best time was set and should be written to the store.
    pub record: bool,
    /// Whether the board's size changed, so the window needs resizing.
    pub resized: bool,
}

impl Reaction {
    /// A reaction that changed the view and nothing else.
    const CHANGED: Self = Self {
        changed: true,
        record: false,
        resized: false,
    };

    /// A reaction that changed nothing.
    const IDLE: Self = Self {
        changed: false,
        record: false,
        resized: false,
    };
}

/// A game, its window, and everything in flight over it.
pub struct Game {
    board: Board,
    difficulty: Difficulty,
    motion: Motion,
    layout: Layout,
    focus: Focus,
    scale: Scale,
    client: Rect,
    /// When the first cell was revealed, so the clock reads from the move that
    /// started the game rather than from when the window opened.
    started_ns: Option<u64>,
    /// The clock's final reading, frozen when the game ended.
    stopped_secs: Option<u32>,
    /// The last whole second drawn, so a tick repaints only when the reading
    /// actually moves.
    shown_secs: u32,
    best: BestTimes,
    /// Whether the question mark is offered in the mark cycle.
    questions: bool,
    /// Where the pointer last was, so a press knows what it landed on.
    pointer: Point,
    /// Whether the press in progress landed on the new-game button.
    face_pressed: bool,
}

impl Game {
    /// A new game of `difficulty`, laid out in `client`.
    #[must_use]
    pub fn new(
        difficulty: Difficulty,
        best: BestTimes,
        questions: bool,
        reduced_motion: bool,
        client: Rect,
        scale: Scale,
    ) -> Self {
        let dims = difficulty.dimensions();
        Self {
            board: Board::new(dims, questions),
            difficulty,
            motion: Motion::new(dims.cols(), reduced_motion),
            layout: Layout::resolve(client, dims, scale),
            focus: Focus::default(),
            scale,
            client,
            started_ns: None,
            stopped_secs: None,
            shown_secs: 0,
            best,
            questions,
            pointer: Point::ORIGIN,
            face_pressed: false,
        }
    }

    /// The board being played.
    #[must_use]
    pub const fn board(&self) -> &Board {
        &self.board
    }

    /// The difficulty in force.
    #[must_use]
    pub const fn difficulty(&self) -> Difficulty {
        self.difficulty
    }

    /// The best times, as the caller should write them.
    #[must_use]
    pub const fn best_times(&self) -> BestTimes {
        self.best
    }

    /// Whether the question mark is offered.
    #[must_use]
    pub const fn questions(&self) -> bool {
        self.questions
    }

    /// The client size this game's board wants, in physical pixels.
    #[must_use]
    pub fn preferred_size(&self) -> (u32, u32) {
        Layout::preferred(self.difficulty.dimensions(), self.scale)
    }

    /// The smallest client size this game's board stays legible in.
    #[must_use]
    pub fn minimum_size(&self) -> (u32, u32) {
        Layout::minimum(self.difficulty.dimensions(), self.scale)
    }

    /// The clock's reading at `now_ns`, in seconds.
    ///
    /// Zero before the first move and frozen at the value it held when the game
    /// ended, so a finished board keeps showing the time that was achieved.
    #[must_use]
    pub fn elapsed_secs(&self, now_ns: u64) -> u32 {
        if let Some(secs) = self.stopped_secs {
            return secs;
        }
        let Some(started) = self.started_ns else {
            return 0;
        };
        let elapsed = now_ns.saturating_sub(started) / SECOND_NS;
        u32::try_from(elapsed)
            .unwrap_or(MAX_TIME_SECS)
            .min(MAX_TIME_SECS)
    }

    /// When the next wake is due, or `None` when the game owes none.
    ///
    /// The earlier of the animation's next frame and the clock's next whole
    /// second. A finished, still board asks for nothing at all, so an idle
    /// window costs no wake and no CPU.
    ///
    /// A clock that has reached the last reading it can show asks for nothing
    /// either: its next second would be an instant already past, and a park to
    /// one of those returns at once and spins.
    #[must_use]
    pub fn deadline_ns(&self, now_ns: u64) -> Option<u64> {
        let animation = self.motion.deadline_ns(now_ns);
        let clock = self
            .started_ns
            .filter(|_| self.stopped_secs.is_none())
            .and_then(|started| {
                let shown = self.elapsed_secs(now_ns);
                (shown < MAX_TIME_SECS)
                    .then(|| started.saturating_add(u64::from(shown + 1) * SECOND_NS))
            });
        match (animation, clock) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (only, None) | (None, only) => only,
        }
    }

    /// Advance the clock and the animation, reporting what must be repainted.
    ///
    /// Called on every wake, timed or not: it is the one place a finished wave
    /// is dropped and a moved clock reading is noticed.
    pub fn tick(&mut self, now_ns: u64, damage: &mut Region) -> bool {
        let mut changed = false;
        if !self.motion.is_idle() {
            for at in self.motion.animating().collect::<Vec<_>>() {
                damage.add(self.layout.cell_damage(at));
            }
            self.motion.advance(now_ns);
            changed = true;
        }
        let secs = self.elapsed_secs(now_ns);
        if secs != self.shown_secs {
            self.shown_secs = secs;
            damage.add(self.layout.clock);
            changed = true;
        }
        changed
    }

    /// Re-lay the game out in a new client rectangle or at a new scale.
    ///
    /// A geometry change moves every pixel, so the whole window is reported.
    pub fn relayout(&mut self, client: Rect, scale: Scale, damage: &mut Region) {
        if self.client == client && self.scale == scale {
            return;
        }
        self.client = client;
        self.scale = scale;
        self.layout = Layout::resolve(client, self.difficulty.dimensions(), scale);
        damage.add(client);
    }

    /// Adopt the theme's reduced-motion policy.
    pub fn set_reduced_motion(&mut self, reduced: bool, damage: &mut Region) {
        if self.motion.reduced_motion() == reduced {
            return;
        }
        self.motion.set_reduced_motion(reduced);
        damage.add(self.client);
    }

    /// Offer, or stop offering, the question mark in the mark cycle.
    pub fn set_questions(&mut self, questions: bool) {
        self.questions = questions;
        self.board.set_questions(questions);
    }

    /// Start a fresh board of the same difficulty.
    pub fn restart(&mut self, damage: &mut Region) {
        self.reset(self.difficulty, damage);
    }

    /// Start a fresh board of `difficulty`, reporting whether the window must
    /// be resized to suit it.
    pub fn set_difficulty(&mut self, difficulty: Difficulty, damage: &mut Region) -> Reaction {
        let resized = difficulty.dimensions() != self.difficulty.dimensions();
        self.reset(difficulty, damage);
        Reaction {
            changed: true,
            record: false,
            resized,
        }
    }

    fn reset(&mut self, difficulty: Difficulty, damage: &mut Region) {
        let dims = difficulty.dimensions();
        self.difficulty = difficulty;
        self.board = Board::new(dims, self.questions);
        self.motion = Motion::new(dims.cols(), self.motion.reduced_motion());
        self.layout = Layout::resolve(self.client, dims, self.scale);
        self.focus = Focus::default();
        self.started_ns = None;
        self.stopped_secs = None;
        self.shown_secs = 0;
        self.face_pressed = false;
        damage.add(self.client);
    }

    /// Draw the current frame.
    pub fn render(&self, surface: &mut Surface, theme: &Theme, font: BitmapFont, now_ns: u64) {
        let skin = Skin::resolve(theme, self.scale, self.layout.cell);
        paint::board(
            surface,
            &self.layout,
            &self.board,
            &self.motion,
            &skin,
            font,
            self.focus,
            self.elapsed_secs(now_ns),
            now_ns,
        );
    }

    // --- Input ----------------------------------------------------------

    /// Route one pointer event.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        now_ns: u64,
        rng: &mut dyn RandU64,
        damage: &mut Region,
    ) -> Reaction {
        match *event {
            InputEvent::PointerMoved { to } => self.hover(to, damage),
            InputEvent::PointerPressed { button } => self.press(button, now_ns, damage),
            InputEvent::PointerReleased { button } => self.release(button, now_ns, rng, damage),
            _ => Reaction::IDLE,
        }
    }

    /// Route one key press.
    pub fn on_key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        now_ns: u64,
        rng: &mut dyn RandU64,
        damage: &mut Region,
    ) -> Reaction {
        match key {
            Key::Named(NamedKey::Left) => self.move_cursor(-1, 0, damage),
            Key::Named(NamedKey::Right) => self.move_cursor(1, 0, damage),
            Key::Named(NamedKey::Up) => self.move_cursor(0, -1, damage),
            Key::Named(NamedKey::Down) => self.move_cursor(0, 1, damage),
            Key::Named(NamedKey::Enter) | Key::Char(' ') => {
                let Some(at) = self.cursor() else {
                    return Reaction::IDLE;
                };
                self.act(at, now_ns, rng, damage)
            }
            Key::Char('f' | 'F') => {
                let Some(at) = self.cursor() else {
                    return Reaction::IDLE;
                };
                let acted = if modifiers.shift {
                    self.board.flag_chord(at)
                } else {
                    self.board.toggle_mark(at)
                };
                self.apply(&acted, at, now_ns, damage)
            }
            Key::Char('n' | 'N') => {
                self.restart(damage);
                Reaction::CHANGED
            }
            Key::Char('1') => self.set_difficulty(Difficulty::Beginner, damage),
            Key::Char('2') => self.set_difficulty(Difficulty::Intermediate, damage),
            Key::Char('3') => self.set_difficulty(Difficulty::Expert, damage),
            _ => Reaction::IDLE,
        }
    }

    /// Where the keyboard is, defaulting to the middle of the board so the
    /// first arrow press has somewhere to move from.
    fn cursor(&mut self) -> Option<Coord> {
        if self.focus.cursor.is_none() {
            let dims = self.board.dimensions();
            self.focus.cursor = Some(Coord::new(dims.cols() / 2, dims.rows() / 2));
        }
        self.focus.cursor
    }

    fn move_cursor(&mut self, dc: i32, dr: i32, damage: &mut Region) -> Reaction {
        let Some(from) = self.cursor() else {
            return Reaction::IDLE;
        };
        let dims = self.board.dimensions();
        let step = |value: u16, delta: i32, limit: u16| -> u16 {
            let moved = i32::from(value) + delta;
            u16::try_from(moved.clamp(0, i32::from(limit).saturating_sub(1))).unwrap_or(value)
        };
        let to = Coord::new(
            step(from.col, dc, dims.cols()),
            step(from.row, dr, dims.rows()),
        );
        if to == from {
            // Already against the edge: the cursor is still drawn where it was,
            // so there is nothing to repaint.
            return Reaction::IDLE;
        }
        self.focus.cursor = Some(to);
        damage.add(self.layout.cell_damage(from));
        damage.add(self.layout.cell_damage(to));
        Reaction::CHANGED
    }

    fn hover(&mut self, to: Point, damage: &mut Region) -> Reaction {
        self.pointer = to;
        let under = self.layout.cell_at(to);
        if under == self.focus.hovered {
            return Reaction::IDLE;
        }
        for cell in [self.focus.hovered, under].into_iter().flatten() {
            damage.add(self.layout.cell_damage(cell));
        }
        self.focus.hovered = under;
        // A press or a chord preview follows the pointer: dragging off the cell
        // that was pressed abandons it, exactly as a button does.
        if self.focus.pressed.is_some() {
            self.focus.pressed = under;
        }
        if let Some(anchor) = self.focus.chording {
            damage.add(self.chord_damage(anchor));
            self.focus.chording = under;
            if let Some(anchor) = under {
                damage.add(self.chord_damage(anchor));
            }
        }
        Reaction::CHANGED
    }

    fn press(&mut self, button: PointerButton, now_ns: u64, damage: &mut Region) -> Reaction {
        let Some(at) = self.focus.hovered else {
            // The only thing outside the grid that answers a click.
            if button == PointerButton::Primary && self.layout.face.contains(self.pointer) {
                self.face_pressed = true;
                damage.add(self.layout.face);
                return Reaction::CHANGED;
            }
            return Reaction::IDLE;
        };
        match button {
            // The press only *shows* the intent; the release commits it, so a
            // pointer dragged off the cell takes the move back.
            PointerButton::Primary => {
                self.focus.pressed = Some(at);
                damage.add(self.layout.cell_damage(at));
                damage.add(self.layout.face);
                Reaction::CHANGED
            }
            // A mark is committed on the press: it is the one action a player
            // repeats quickly, and waiting for the release makes it feel slow.
            PointerButton::Secondary => {
                let acted = if self.board.cover(at) == Some(Cover::Open) {
                    self.board.flag_chord(at)
                } else {
                    self.board.toggle_mark(at)
                };
                self.apply(&acted, at, now_ns, damage)
            }
            PointerButton::Middle => {
                self.focus.chording = Some(at);
                damage.add(self.chord_damage(at));
                damage.add(self.layout.face);
                Reaction::CHANGED
            }
        }
    }

    fn release(
        &mut self,
        button: PointerButton,
        now_ns: u64,
        rng: &mut dyn RandU64,
        damage: &mut Region,
    ) -> Reaction {
        match button {
            PointerButton::Primary => {
                let Some(at) = self.focus.pressed.take() else {
                    return self.release_off_grid(damage);
                };
                damage.add(self.layout.cell_damage(at));
                damage.add(self.layout.face);
                self.act(at, now_ns, rng, damage)
            }
            PointerButton::Middle => {
                let Some(at) = self.focus.chording.take() else {
                    return Reaction::IDLE;
                };
                damage.add(self.chord_damage(at));
                damage.add(self.layout.face);
                let acted = self.board.chord(at);
                self.apply(&acted, at, now_ns, damage)
            }
            PointerButton::Secondary => Reaction::IDLE,
        }
    }

    /// A primary release with no pressed cell.
    ///
    /// Only a press that landed on the new-game button and was released over it
    /// starts a new game; a press dragged off the button, or off the grid, is
    /// abandoned exactly as a button's would be.
    fn release_off_grid(&mut self, damage: &mut Region) -> Reaction {
        if !core::mem::take(&mut self.face_pressed) {
            return Reaction::IDLE;
        }
        damage.add(self.layout.face);
        if !self.layout.face.contains(self.pointer) {
            return Reaction::CHANGED;
        }
        self.restart(damage);
        Reaction::CHANGED
    }

    /// Reveal or chord `at`, whichever the cell calls for.
    ///
    /// An open number is chorded rather than ignored, because a click on one is
    /// only ever a request to open what it says is safe.
    fn act(
        &mut self,
        at: Coord,
        now_ns: u64,
        rng: &mut dyn RandU64,
        damage: &mut Region,
    ) -> Reaction {
        let acted = if self.board.cover(at) == Some(Cover::Open) {
            self.board.chord(at)
        } else {
            if self.board.phase() == Phase::Ready {
                self.started_ns = Some(now_ns);
            }
            self.board.reveal(at, rng)
        };
        self.apply(&acted, at, now_ns, damage)
    }

    /// Adopt a board action: damage what it changed, start its wave, and settle
    /// the clock and the best time when it ended the game.
    fn apply(&mut self, acted: &Move, at: Coord, now_ns: u64, damage: &mut Region) -> Reaction {
        if acted.is_nothing() {
            // A refused action is an answer, so the cell says so rather than
            // the click vanishing.
            return self.refuse(at, now_ns, damage);
        }
        for step in &acted.steps {
            damage.add(self.layout.cell_damage(step.at));
        }
        damage.add(self.layout.counter);
        damage.add(self.layout.face);

        // The winning move is a reveal like any other; the sweep over the
        // finished board below is a wave of its own.
        let kind = match acted.outcome {
            Outcome::Opened | Outcome::Won => WaveKind::Reveal,
            Outcome::Marked => WaveKind::Mark,
            Outcome::Detonated => WaveKind::Detonate,
            Outcome::Nothing => return Reaction::CHANGED,
        };
        self.motion.begin(kind, &acted.steps, now_ns);

        if !self.board.phase().is_over() {
            return Reaction::CHANGED;
        }
        let secs = self.elapsed_secs(now_ns);
        self.stopped_secs = Some(secs);
        damage.add(self.layout.clock);
        if acted.outcome != Outcome::Won {
            return Reaction::CHANGED;
        }
        // The victory sweep runs over the whole board, outward from where the
        // winning move landed, so it is a wave of its own rather than a second
        // meaning for the reveal.
        self.motion
            .begin(WaveKind::Victory, &self.sweep(at), now_ns);
        damage.add(self.client);
        let record = self.best.record(self.difficulty, secs);
        Reaction {
            changed: true,
            record,
            resized: false,
        }
    }

    /// Report a refused action by shaking the cell it was refused on.
    fn refuse(&mut self, at: Coord, now_ns: u64, damage: &mut Region) -> Reaction {
        if !self.board.contains(at) || self.board.phase().is_over() {
            return Reaction::IDLE;
        }
        damage.add(self.layout.cell_damage(at));
        self.motion
            .begin(WaveKind::Rejected, &[Step { at, ring: 0 }], now_ns);
        Reaction::CHANGED
    }

    /// Every cell, ringed outward from `origin`: the victory sweep's shape.
    fn sweep(&self, origin: Coord) -> Vec<Step> {
        self.board
            .iter()
            .map(|(at, _, _)| Step {
                at,
                ring: at.col.abs_diff(origin.col).max(at.row.abs_diff(origin.row)),
            })
            .collect()
    }

    /// The anchor of a chord preview and the ring of cells it presses.
    fn chord_damage(&self, anchor: Coord) -> Rect {
        let mut area = self.layout.cell_damage(anchor);
        for (dc, dr) in [(-1_i32, -1_i32), (1, 1)] {
            let col = i32::from(anchor.col) + dc;
            let row = i32::from(anchor.row) + dr;
            let Ok(col) = u16::try_from(col.max(0)) else {
                continue;
            };
            let Ok(row) = u16::try_from(row.max(0)) else {
                continue;
            };
            let corner = self.layout.cell_damage(Coord::new(
                col.min(self.board.dimensions().cols().saturating_sub(1)),
                row.min(self.board.dimensions().rows().saturating_sub(1)),
            ));
            if !corner.is_empty() {
                area = area.union(&corner);
            }
        }
        area
    }
}

#[cfg(test)]
#[path = "game_tests.rs"]
mod tests;
