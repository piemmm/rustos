//! The account chooser: the tiles a login screen offers, the one grid
//! geometry its paint and its hit test share, and the focus model that keeps
//! it operable with no pointer at all.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_controls::{ControlState, FocusState, IconTile, PointerState, SelectionState};
use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_icon::IconKind;
use tairix_input::{InputEvent, PointerButton};
use tairix_raster::{Color, Surface};
use tairix_theme::{Rgba, TextRole, Theme};

use crate::layout::{centre_on, down, Column, CHOOSER_HINT_GAP, NOTICE_BAND, SIDE_MARGIN};

/// The trailing tile that leads to a typed login name.
pub(crate) const OTHER_LABEL: &str = "Other…";

/// The mark on the trailing tile's disc — the ellipsis its label ends with.
///
/// It wears a disc like every account so it reads as a peer of them rather
/// than as a leftover, in the quiet plate colours rather than the accent, so
/// it is still plainly not one of the listed people.
pub(crate) const OTHER_MONOGRAM: char = '\u{2026}';

/// The monogram drawn for an account whose name yields no character.
pub(crate) const FALLBACK_MONOGRAM: char = '?';

/// One tile's width in pixels at the reference density.
pub(crate) const TILE_WIDTH: u32 = 132;

/// One tile's height in pixels at the reference density: the monogram disc
/// and the name under it.
///
/// Sized so the band under the disc holds three whole label lines at the
/// reference density and at a doubled one, which is capacity rather than
/// layout — a one-word name still draws one line. Two lines would leave a
/// long single word (`Administrator`) with no room to fall past, so a face
/// wider than the reference one would break it mid-word and elide it.
pub(crate) const TILE_HEIGHT: u32 = 154;

/// The gap between tiles in pixels at the reference density.
pub(crate) const TILE_GAP: u32 = 12;

/// Frames spanned by one selection cross-fade.
///
/// Eight frames over a hundred milliseconds reads as smooth without inventing
/// a frame clock.
const SELECTION_FADE_FRAMES: u64 = 8;

/// Nanoseconds in one millisecond.
const NANOS_PER_MS: u64 = 1_000_000;

/// One selectable account on the chooser.
///
/// It carries only what is drawn. There is no credential material here, no
/// capability set, and no home path: an unauthenticated screen is shown this
/// list, so anything it holds is something an onlooker can read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountTile {
    display_name: String,
    login_name: String,
    live: bool,
}

impl AccountTile {
    /// A tile for `login_name`, labelled `display_name`.
    ///
    /// An empty display name falls back to the login name, so an account the
    /// authority could not describe still reads as itself rather than as a
    /// blank tile.
    #[must_use]
    pub fn new(display_name: &str, login_name: &str) -> Self {
        let shown = if display_name.is_empty() {
            login_name
        } else {
            display_name
        };
        Self {
            display_name: shown.to_string(),
            login_name: login_name.to_string(),
            live: false,
        }
    }

    /// This tile marked as already having a session running behind the login
    /// screen, which the chooser badges.
    #[must_use]
    pub fn with_live_session(mut self, live: bool) -> Self {
        self.live = live;
        self
    }

    /// The name shown on the tile.
    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// The login name a secret is asked for once this tile is chosen.
    #[must_use]
    pub fn login_name(&self) -> &str {
        &self.login_name
    }

    /// Whether this account already has a session running.
    #[must_use]
    pub fn has_live_session(&self) -> bool {
        self.live
    }

    /// The character on the tile's disc: the first of the shown name,
    /// uppercased, or a fallback glyph when there is no name at all.
    #[must_use]
    pub fn monogram(&self) -> char {
        monogram_of(&self.display_name)
    }
}

/// The disc mark for `name`: its first character uppercased, or
/// [`FALLBACK_MONOGRAM`] when there is nothing to take one from.
///
/// Shared by the chooser's tiles and the prompt's larger disc, so the person
/// sees the same mark before and after picking an account. A scalar whose
/// uppercase form is several characters (`ß`) contributes the first of them,
/// since only one is drawn.
pub(crate) fn monogram_of(name: &str) -> char {
    name.chars().next().map_or(FALLBACK_MONOGRAM, |ch| {
        ch.to_uppercase().next().unwrap_or(ch)
    })
}

/// Private record of a selection-mark cross-fade between two tiles.
///
/// One consumer only — this chooser — so it stays here rather than becoming
/// shared surface.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SelectionFade {
    /// Slot the mark is leaving, if any.
    leaving: Option<usize>,
    /// Slot the mark is arriving at.
    arriving: usize,
    /// Monotonic time the transition started, in nanoseconds.
    started_ns: u64,
    /// How long the transition runs, in nanoseconds.
    duration_ns: u64,
    /// Strength of the arriving mark: `0` unmarked, [`u8::MAX`] full.
    arriving_fade: u8,
    /// Whether a transition is currently running.
    running: bool,
}

impl SelectionFade {
    /// Settled on `slot`: fully marked, nothing leaving, nothing running.
    const fn settled_on(slot: usize) -> Self {
        Self {
            leaving: None,
            arriving: slot,
            started_ns: 0,
            duration_ns: 0,
            arriving_fade: u8::MAX,
            running: false,
        }
    }

    /// Begin a transition from `from` to `to` at `now_ns`.
    ///
    /// A zero duration settles immediately: the new slot is full, the old is
    /// gone, and nothing is left running.
    fn start(&mut self, from: usize, to: usize, now_ns: u64, duration_ms: u16) {
        let span_ns = u64::from(duration_ms).saturating_mul(NANOS_PER_MS);
        if span_ns == 0 {
            *self = Self::settled_on(to);
            return;
        }
        self.leaving = Some(from);
        self.arriving = to;
        self.started_ns = now_ns;
        self.duration_ns = span_ns;
        self.arriving_fade = 0;
        self.running = true;
    }

    /// End the transition on `slot`: full mark, nothing leaving.
    fn settle(&mut self, slot: usize) {
        *self = Self::settled_on(slot);
    }

    /// Strength of the mark on `slot`.
    fn strength(&self, slot: usize, focus: usize) -> u8 {
        if self.running {
            if slot == self.arriving {
                return self.arriving_fade;
            }
            if self.leaving == Some(slot) {
                return u8::MAX.saturating_sub(self.arriving_fade);
            }
            return 0;
        }
        if slot == focus {
            u8::MAX
        } else {
            0
        }
    }

    /// Recompute strengths from elapsed time. Returns whether anything
    /// changed. A clock that jumps backwards settles rather than misbehaving.
    fn advance(&mut self, now_ns: u64, focus: usize) -> bool {
        if !self.running {
            return false;
        }
        if now_ns < self.started_ns {
            self.settle(focus);
            return true;
        }
        let elapsed = now_ns.saturating_sub(self.started_ns);
        if elapsed >= self.duration_ns {
            self.settle(focus);
            return true;
        }
        let progress = elapsed
            .saturating_mul(u64::from(u8::MAX))
            .checked_div(self.duration_ns)
            .unwrap_or(u64::from(u8::MAX));
        let fade = u8::try_from(progress.min(u64::from(u8::MAX))).unwrap_or(u8::MAX);
        if fade == self.arriving_fade {
            return false;
        }
        self.arriving_fade = fade;
        true
    }

    /// Nanoseconds until the next fade frame, or `None` when nothing is
    /// animating.
    fn next_frame_in(&self, now_ns: u64) -> Option<u64> {
        if !self.running {
            return None;
        }
        let elapsed = now_ns.saturating_sub(self.started_ns);
        let remaining = self.duration_ns.saturating_sub(elapsed);
        if remaining == 0 {
            return Some(0);
        }
        let step = (self.duration_ns / SELECTION_FADE_FRAMES).max(1);
        Some(remaining.min(step))
    }
}

/// The chooser's tiles and where the keyboard is.
///
/// Slot `accounts.len()` is the `Other…` tile: it is always present and
/// always last, so an account the authority did not list — or a machine with
/// no accounts to list at all — is still reachable by typing a name.
pub(crate) struct Chooser {
    accounts: Vec<AccountTile>,
    focus: usize,
    fade: SelectionFade,
    /// Slots whose mark strength changed on the last focus move, for damage.
    focus_damage: ([usize; 4], usize),
    armed: Option<usize>,
    pointer: Point,
}

impl Chooser {
    pub(crate) fn new(accounts: Vec<AccountTile>) -> Self {
        Self {
            accounts,
            focus: 0,
            fade: SelectionFade::settled_on(0),
            focus_damage: ([0; 4], 0),
            armed: None,
            pointer: Point::ORIGIN,
        }
    }

    /// The tiles, `Other…` included.
    pub(crate) fn slots(&self) -> usize {
        self.accounts.len().saturating_add(1)
    }

    /// The slot the keyboard is on.
    pub(crate) fn focus(&self) -> usize {
        self.focus
    }

    /// The account at `slot`, or `None` for the `Other…` tile.
    pub(crate) fn account(&self, slot: usize) -> Option<&AccountTile> {
        self.accounts.get(slot)
    }

    /// Move the keyboard on by `step` slots, wrapping at both ends so a
    /// keyboard-only user never falls off the end of the row.
    pub(crate) fn move_focus(&mut self, step: Step, now_ns: u64, duration_ms: u16) -> bool {
        let slots = self.slots();
        let next = match step {
            Step::Next => self.focus.saturating_add(1) % slots,
            Step::Previous => self.focus.checked_sub(1).unwrap_or(slots - 1),
        };
        self.focus_on(next, now_ns, duration_ms)
    }

    /// Put the keyboard on `slot`, ignoring a slot that does not exist.
    ///
    /// When the focus actually moves, starts a selection cross-fade lasting
    /// `duration_ms`. A zero duration settles immediately.
    pub(crate) fn focus_on(&mut self, slot: usize, now_ns: u64, duration_ms: u16) -> bool {
        if slot >= self.slots() || slot == self.focus {
            return false;
        }
        let from = self.focus;
        // Every slot whose mark strength will change must be in the damage
        // report: the one left, the one arrived at, and any prior fade pair
        // a second move interrupts (those drop to zero instantly).
        let mut dirty = [from, slot, 0, 0];
        let mut n = 2usize;
        if self.fade.running {
            if let Some(leaving) = self.fade.leaving {
                if leaving != from && leaving != slot {
                    dirty[n] = leaving;
                    n += 1;
                }
            }
            let arriving = self.fade.arriving;
            if arriving != from && arriving != slot {
                dirty[n] = arriving;
                n += 1;
            }
        }
        self.focus = slot;
        self.fade.start(from, slot, now_ns, duration_ms);
        self.focus_damage = (dirty, n);
        true
    }

    /// Recompute fade strengths from `now_ns`. Returns whether anything
    /// changed. Completing the transition settles it so nothing keeps
    /// animating.
    pub(crate) fn advance(&mut self, now_ns: u64) -> bool {
        self.fade.advance(now_ns, self.focus)
    }

    /// Nanoseconds until the next fade frame, or `None` when nothing is
    /// animating.
    #[must_use]
    pub(crate) fn next_frame_in(&self, now_ns: u64) -> Option<u64> {
        self.fade.next_frame_in(now_ns)
    }

    /// The mark strength tile `slot` draws at: `0` unmarked, [`u8::MAX`] full.
    #[must_use]
    pub(crate) fn selection_fade(&self, slot: usize) -> u8 {
        self.fade.strength(slot, self.focus)
    }

    /// Union of the rectangles of `slots`, using the same geometry the paint
    /// draws into. `None` when none of the slots exist.
    #[must_use]
    pub(crate) fn tile_bounds_of(
        &self,
        slots: &[usize],
        screen: Rect,
        scale: Scale,
    ) -> Option<Rect> {
        let mut union: Option<Rect> = None;
        for &slot in slots {
            let Some(rect) = self.tile_rect(slot, screen, scale) else {
                continue;
            };
            union = Some(match union {
                Some(acc) => acc.union(&rect),
                None => rect,
            });
        }
        union
    }

    /// Slots a running fade currently paints: leaving (if any) and arriving.
    /// Empty when nothing is animating.
    #[must_use]
    pub(crate) fn animating_slots(&self) -> ([usize; 2], usize) {
        if !self.fade.running {
            return ([0, 0], 0);
        }
        let mut slots = [self.fade.arriving, 0];
        if let Some(leaving) = self.fade.leaving {
            slots[1] = leaving;
            ([slots[0], slots[1]], 2)
        } else {
            (slots, 1)
        }
    }

    /// The tiles a running fade touches, as a union rectangle from the paint
    /// geometry. `None` when nothing is animating.
    #[must_use]
    pub(crate) fn fade_damage(&self, screen: Rect, scale: Scale) -> Option<Rect> {
        let (slots, count) = self.animating_slots();
        if count == 0 {
            return None;
        }
        self.tile_bounds_of(&slots[..count], screen, scale)
    }

    /// Damage rectangle for the last focus move: every tile whose mark
    /// strength changed, including a prior fade pair a second move cut short.
    #[must_use]
    pub(crate) fn focus_move_damage(&self, screen: Rect, scale: Scale) -> Option<Rect> {
        let (slots, count) = self.focus_damage;
        if count == 0 {
            return self.fade_damage(screen, scale);
        }
        self.tile_bounds_of(&slots[..count], screen, scale)
    }

    /// How many tiles sit in one row on `screen`, never fewer than one.
    ///
    /// As many as the screen holds inside its side margins, so the accounts
    /// stay one centred row and wrap into a grid only when they must.
    fn columns(&self, screen: Rect, scale: Scale) -> usize {
        let gap = scale.scale_length(TILE_GAP);
        let stride = scale.scale_length(TILE_WIDTH).saturating_add(gap).max(1);
        let room = screen
            .width
            .saturating_sub(scale.scale_length(SIDE_MARGIN).saturating_mul(2));
        let fits = room.saturating_add(gap) / stride;
        usize::try_from(fits).unwrap_or(1).clamp(1, self.slots())
    }

    /// The tile grid's own width and height on `screen`.
    fn grid_size(&self, screen: Rect, scale: Scale) -> (u32, u32) {
        let columns = self.columns(screen, scale);
        let rows = self.slots().div_ceil(columns);
        let gap = scale.scale_length(TILE_GAP);
        (
            span(columns, scale.scale_length(TILE_WIDTH), gap).min(screen.width),
            span(rows, scale.scale_length(TILE_HEIGHT), gap).min(screen.height),
        )
    }

    /// The whole chooser body: the grid and the hint line under it, which is
    /// what the shared column centres in the space below the chrome.
    pub(crate) fn body_height(&self, screen: Rect, scale: Scale) -> u32 {
        self.grid_size(screen, scale)
            .1
            .saturating_add(scale.scale_length(CHOOSER_HINT_GAP))
            .saturating_add(scale.scale_length(NOTICE_BAND))
    }

    /// The rectangle the whole grid covers on `screen`.
    ///
    /// The one definition of where the chooser is: the paint, the pointer hit
    /// test, and the damage report all read it, so they cannot drift apart.
    pub(crate) fn bounds(&self, screen: Rect, scale: Scale) -> Rect {
        let (w, h) = self.grid_size(screen, scale);
        let column = Column::new(screen, scale, self.body_height(screen, scale));
        Rect::new(
            centre_on(screen.origin.x, screen.width, w),
            column.body_top,
            w,
            h,
        )
    }

    /// The full-width band the chooser's one hint line sits in, under the
    /// grid.
    pub(crate) fn hint_rect(&self, screen: Rect, scale: Scale) -> Rect {
        let grid = self.bounds(screen, scale);
        Rect::new(
            screen.origin.x,
            down(
                down(grid.origin.y, grid.height),
                scale.scale_length(CHOOSER_HINT_GAP),
            ),
            screen.width,
            scale.scale_length(NOTICE_BAND),
        )
    }

    /// Where tile `slot` sits on `screen`, or `None` for a slot the chooser
    /// does not have. A short final row is centred under the ones above it.
    pub(crate) fn tile_rect(&self, slot: usize, screen: Rect, scale: Scale) -> Option<Rect> {
        let slots = self.slots();
        if slot >= slots {
            return None;
        }
        let columns = self.columns(screen, scale);
        let grid = self.bounds(screen, scale);
        let tile_w = scale.scale_length(TILE_WIDTH);
        let tile_h = scale.scale_length(TILE_HEIGHT);
        let gap = scale.scale_length(TILE_GAP);
        let row = slot / columns;
        let column = slot % columns;
        let row_w = span(
            slots.saturating_sub(row * columns).min(columns),
            tile_w,
            gap,
        );
        let stride = |steps: usize, extent: u32| {
            u32::try_from(steps)
                .unwrap_or(0)
                .saturating_mul(extent.saturating_add(gap))
        };
        Some(Rect::new(
            advance(
                centre_on(grid.origin.x, grid.width, row_w),
                stride(column, tile_w),
            ),
            advance(grid.origin.y, stride(row, tile_h)),
            tile_w,
            tile_h,
        ))
    }

    /// The slot under `point`, tested against the very rectangles the paint
    /// draws into.
    pub(crate) fn hit(&self, point: Point, screen: Rect, scale: Scale) -> Option<usize> {
        (0..self.slots()).find(|&slot| {
            self.tile_rect(slot, screen, scale)
                .is_some_and(|rect| rect.contains(point))
        })
    }

    /// Track the pointer and report the slot a completed primary click
    /// activated.
    ///
    /// A press arms the tile under the pointer and a release over that same
    /// tile activates it, so a press that slides off before it is let go
    /// chooses nothing.
    pub(crate) fn on_pointer(
        &mut self,
        event: &InputEvent,
        screen: Rect,
        scale: Scale,
        now_ns: u64,
        duration_ms: u16,
    ) -> (Option<usize>, bool) {
        match event {
            InputEvent::PointerMoved { to } => {
                self.pointer = *to;
                let over = self.hit(*to, screen, scale);
                (
                    None,
                    over.is_some_and(|slot| self.focus_on(slot, now_ns, duration_ms)),
                )
            }
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => {
                self.armed = self.hit(self.pointer, screen, scale);
                if let Some(slot) = self.armed {
                    self.focus_on(slot, now_ns, duration_ms);
                }
                (None, self.armed.is_some())
            }
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => {
                let armed = self.armed.take();
                let over = self.hit(self.pointer, screen, scale);
                let chosen = armed.filter(|slot| Some(*slot) == over);
                (chosen, armed.is_some())
            }
            _ => (None, false),
        }
    }

    /// Paint every tile on `screen`.
    pub(crate) fn render(&self, surface: &mut Surface, screen: Rect, scale: Scale, theme: &Theme) {
        for slot in 0..self.slots() {
            let Some(bounds) = self.tile_rect(slot, screen, scale) else {
                continue;
            };
            let account = self.account(slot);
            let tile = IconTile::new(
                account.map_or(OTHER_LABEL, AccountTile::display_name),
                IconKind::Generic,
            )
            .with_state(self.tile_state(slot))
            .with_selection_fade(self.selection_fade(slot));
            let palette = theme.palette();
            let (monogram, disc, ink) = match account {
                Some(account) => (account.monogram(), palette.accent, palette.on_accent),
                None => (OTHER_MONOGRAM, palette.surface_raised, palette.on_surface),
            };
            let artwork = monogram_disc(
                monogram,
                IconTile::icon_side(bounds, scale, theme),
                BitmapFont::for_role(theme.fonts(), TextRole::Heading, scale),
                (disc, ink),
            );
            tile.render(surface, bounds, scale, theme, artwork.as_ref());
            if account.is_some_and(AccountTile::has_live_session) {
                paint_live_badge(surface, bounds, scale, theme);
            }
        }
    }

    /// The composed state tile `slot` draws in.
    fn tile_state(&self, slot: usize) -> ControlState {
        let focused = slot == self.focus();
        ControlState::idle()
            .with_selection(if focused {
                SelectionState::Selected
            } else {
                SelectionState::Unselected
            })
            .with_pointer(if self.armed == Some(slot) {
                PointerState::Pressed
            } else {
                PointerState::None
            })
            .with_focus(FocusState {
                focused,
                in_focus_field: true,
            })
    }
}

/// Which way [`Chooser::move_focus`] takes the keyboard.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum Step {
    /// On to the following slot, wrapping back to the first.
    Next,
    /// Back to the preceding slot, wrapping round to the last.
    Previous,
}

/// The extent `count` tiles of `tile` pixels separated by `gap` cover.
fn span(count: usize, tile: u32, gap: u32) -> u32 {
    let count = u32::try_from(count).unwrap_or(0);
    count
        .saturating_mul(tile)
        .saturating_add(count.saturating_sub(1).saturating_mul(gap))
}

/// `origin` moved on by `offset` pixels.
fn advance(origin: i32, offset: u32) -> i32 {
    origin.saturating_add(i32::try_from(offset).unwrap_or(i32::MAX))
}

/// A `side`×`side` disc bearing `monogram`, in the `(fill, ink)` colours the
/// caller chose.
///
/// The one disc definition the chooser's tiles and the prompt's larger disc
/// share, so the mark a person picks is the mark they then see. The caller
/// chooses the text role `font` comes from, since a tile's disc and the
/// prompt's larger one carry the mark at different sizes. Produced at exactly
/// the side asked for, so it can never be scaled or cropped by whatever
/// places it. `None` when there is no room for a picture at all, which leaves
/// an icon tile drawing its fallback glyph.
pub(crate) fn monogram_disc(
    monogram: char,
    side: u32,
    font: BitmapFont,
    (fill, ink): (Rgba, Rgba),
) -> Option<Surface> {
    if side == 0 {
        return None;
    }
    let mut disc = Surface::new(side, side)?;
    disc.fill_round_rect(0, 0, side, side, side / 2, Color::from(fill));

    let mut encoded = [0u8; 4];
    let text = &*monogram.encode_utf8(&mut encoded);
    let width = font.text_width(text).min(side);
    let height = font.line_height().min(side);
    font.draw_text(
        &mut disc,
        i32::try_from((side - width) / 2).unwrap_or(0),
        i32::try_from((side - height) / 2).unwrap_or(0),
        text,
        Color::from(ink),
    );
    Some(disc)
}

/// Mark a tile whose account already has a session running, in the tile
/// vocabulary's own top-trailing signal corner.
fn paint_live_badge(surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
    let pad = scale.scale_length(theme.metrics().control_inset).max(1);
    let size = scale
        .scale_length(theme.metrics().bead_size)
        .max(3)
        .min(bounds.width)
        .min(bounds.height);
    let Ok(x) = u32::try_from(bounds.origin.x) else {
        return;
    };
    let Ok(y) = u32::try_from(bounds.origin.y) else {
        return;
    };
    let bx = x
        .saturating_add(bounds.width)
        .saturating_sub(size)
        .saturating_sub(pad);
    surface.fill_round_rect(
        bx,
        y.saturating_add(pad),
        size,
        size,
        size / 2,
        Color::from(theme.palette().success),
    );
}
