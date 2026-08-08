//! The authentication surface: the account chooser, the panel, the masked
//! field, the notice, and the state machine that turns one input event into a
//! verdict.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::Cell;

use tairix_abi::Duration64;
use tairix_controls::{PointerState, TextAction, TextField, ValidationState};
use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Scale};
use tairix_icon::{builtin_icon, IconKind};
use tairix_input::{InputEvent, Key, NamedKey};
use tairix_raster::{Color, Surface};
use tairix_theme::{Contrast, MotionInteraction, TextRole, Theme};

use crate::chooser::{monogram_disc, monogram_of, AccountTile, Chooser, Step, OTHER_MONOGRAM};
use crate::layout::{
    back_band, centre_on, chrome_band, chrome_bands, draw_centred, notice_band, Prompt, FIELD_WIDTH,
};
use crate::motion::{
    at_strength, between_rects, fade, sooner, travelling_font, Changed, Shake, Stage, Toward, Veil,
    VEIL,
};

/// Longest secret the surface will hold, in characters.
///
/// The same bound the text login prompt reads a line at, and far below what
/// the elevation wire format accepts. It is a fail-closed memory bound, not
/// a policy on what a password may be: it exists so the field can reserve
/// its buffer once and never grow it, which is what keeps a copy of the
/// secret out of a freed heap block.
pub const MAX_PASSWORD: usize = 256;

/// Longest login name the `Other…` field will hold, in characters.
///
/// A fail-closed memory bound like [`MAX_PASSWORD`], not a naming policy:
/// the authority decides what a login name may be, and refuses one it does
/// not recognise.
pub const MAX_LOGIN_NAME: usize = 64;

/// Longest clock, date, or host string the backdrop will draw, in
/// characters. Anything longer is truncated rather than allowed to run off
/// the screen.
pub const MAX_CHROME: usize = 64;

/// The heading shown when the embedder could not name the account.
///
/// The surface still asks: whose secret is wanted is a nicety, and failing
/// to resolve it must never be a reason to let anybody through.
pub const UNNAMED_ACCOUNT: &str = "Locked";

/// What the field says while it is waiting for a secret.
pub(crate) const HINT: &str = "Type your password and press Enter";

/// What the field says after the verifier refused the secret.
pub(crate) const REFUSED: &str = "That password was not accepted";

/// What the field says when no answer could be obtained at all.
///
/// Deliberately distinct from a refusal: "wrong password" and "I could not
/// ask" call for different reactions from the person at the keyboard.
pub(crate) const UNREACHABLE: &str = "Your password could not be checked just now";

/// What the chooser says while it is waiting for an account to be picked.
pub(crate) const CHOOSE_HINT: &str = "Choose an account";

/// The heading over the typed-login-name field.
pub(crate) const NAME_HEADING: &str = "Log in";

/// What the login-name field says while it is waiting for a name.
pub(crate) const NAME_HINT: &str = "Type your login name and press Enter";

/// What the login-name field says when it was submitted empty.
pub(crate) const NAME_REQUIRED: &str = "Type a login name";

/// What the prompt says when there is a chooser to step back to, so leaving
/// is never a thing the person has to guess at.
pub(crate) const BACK_HINT: &str = "Press Escape to choose another account";

/// How much of the theme's desktop colour the legibility wash lays over the
/// very top and bottom of the backdrop.
const WASH_ALPHA: u8 = 140;

/// The share of the screen's height each end of that wash covers.
const WASH_BANDS: u32 = 3;

/// The pill's edge, as a multiple of the theme's rim thickness: enough to
/// stand in for the shared field's own rim, the gap inside it, and its focus
/// ring, all three of which the pill's mask removes.
const PILL_EDGE_RIMS: u32 = 3;

/// What an authority answered about one offered secret.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// The secret belongs to the account. This is the only answer that lets
    /// anybody through.
    Verified,
    /// A real answer about a real secret: it is not the account's.
    Refused,
    /// No answer could be obtained — nothing listening, a transport fault,
    /// a reply that is not this protocol. Never mistaken for a refusal, and
    /// never for a pass.
    Unreachable,
}

/// How the surface asks whether a secret belongs to an account.
///
/// The surface never answers that itself: it holds no credential store, no
/// key, and no authority. On a running system the embedder implements this
/// over whatever authenticates for it — the per-console elevation broker
/// behind the desktop's screen lock, the session authority behind the login
/// greeter — and a test injects an answer directly. Holding it behind a seam
/// is what lets the whole surface — its editing, its wording, its erasure of
/// the secret — be exercised on the host without a kernel.
pub trait Verifier {
    /// Whether `secret` authenticates `account`.
    ///
    /// `account` is the login name the surface is currently asking for. An
    /// embedder that authenticates its own kernel-attested caller, rather
    /// than a name it was handed, ignores it — asking on behalf of another
    /// account is exactly what such a broker must refuse.
    fn verify(&mut self, account: &str, secret: &str) -> Verdict;
}

/// The clock, date, and host name drawn on the backdrop above the panel.
///
/// Display text and nothing else. It is drawn on an unauthenticated screen,
/// so it carries no authority and is never read back for one; the strings
/// are truncated to [`MAX_CHROME`] when they are set.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Chrome {
    /// The time of day, as the embedder chose to spell it.
    pub clock: String,
    /// The date, as the embedder chose to spell it.
    pub date: String,
    /// This machine's name.
    pub host: String,
}

impl Chrome {
    /// This chrome with every line cut to [`MAX_CHROME`] characters.
    fn bounded(self) -> Self {
        Self {
            clock: cut(self.clock),
            date: cut(self.date),
            host: cut(self.host),
        }
    }

    /// Whether there is anything at all to draw.
    fn is_empty(&self) -> bool {
        self.clock.is_empty() && self.date.is_empty() && self.host.is_empty()
    }
}

/// `text` cut to [`MAX_CHROME`] characters.
fn cut(text: String) -> String {
    match text.char_indices().nth(MAX_CHROME) {
        Some((end, _)) => text[..end].to_string(),
        None => text,
    }
}

/// What the surface paints behind its panel.
///
/// The caller chooses it, so an embedder that already holds a decoded
/// wallpaper can hand one in without this engine ever learning to decode or
/// fit an image: that is the caller's sandboxed business, painting is this
/// crate's.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Backdrop<'a> {
    /// The active theme's flat desktop colour, fully opaque.
    Desktop,
    /// An already-decoded, already-fitted wallpaper under the theme's scrim.
    Wallpaper {
        /// The picture, in the same coordinates the frame is painted in.
        image: &'a Surface,
        /// How much of the theme's desktop colour is laid over the picture,
        /// from [`scrim_alpha`](crate::scrim_alpha). At full opacity this is
        /// exactly [`Backdrop::Desktop`], so the wallpaper can never be the
        /// reason text stops being legible.
        scrim: u8,
    },
}

/// What one input event, or one update from the embedder, did to the surface.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Outcome {
    redraw: bool,
    verified: bool,
    damage: Option<Rect>,
}

impl Outcome {
    /// Nothing changed: no repaint, and no pixel to repaint.
    const fn quiet() -> Self {
        Self {
            redraw: false,
            verified: false,
            damage: Some(Rect::EMPTY),
        }
    }

    /// A repaint of `damage`, or of the whole screen when the surface does
    /// not know where it is.
    const fn changed(damage: Option<Rect>) -> Self {
        Self {
            redraw: true,
            verified: false,
            damage,
        }
    }

    /// Whether the frame on screen is now stale and the embedder must
    /// repaint.
    #[must_use]
    pub const fn redraw(self) -> bool {
        self.redraw
    }

    /// Whether a secret was offered and the verifier accepted it, by which
    /// time the secret has already been erased.
    #[must_use]
    pub const fn verified(self) -> bool {
        self.verified
    }

    /// The rectangle whose pixels the next paint changes, or `None` for "the
    /// whole screen".
    ///
    /// `None` is the honest answer whenever the change is not confined to one
    /// part of the surface — a mode change, a verified secret — or whenever
    /// the surface has not yet been told where it is (it learns that from the
    /// frames it paints and the events it answers). An empty rectangle means
    /// the next paint changes nothing at all.
    #[must_use]
    pub fn damage(&self) -> Option<Rect> {
        self.damage
    }
}

/// Everything the surface needs to place, hit-test, and answer one event.
///
/// Bundled rather than passed loose because all four are read together on
/// every event, and because the same screen, scale, and theme must reach
/// the hit test that reached the paint.
pub struct EventContext<'a> {
    /// The rectangle the surface's own pixels cover, in the surface's own
    /// coordinate space — the same rectangle the frame was rendered for.
    pub screen: Rect,
    /// The desktop scale the frame was rendered at.
    pub scale: Scale,
    /// The theme the frame was rendered in.
    pub theme: &'a Theme,
    /// Who decides whether a submitted secret belongs to the account.
    pub verifier: &'a mut dyn Verifier,
    /// Monotonic nanoseconds the surface times its motion from: the chooser's
    /// selection cross-fade, one stage giving way to another, and the shake
    /// that answers a rejected attempt.
    pub now_ns: u64,
}

/// What the surface is currently asking for.
enum Mode {
    /// Which account is this? Only reachable when there are tiles to choose
    /// between.
    Chooser,
    /// Which account is this, spelled out? The field a chooser's `Other…`
    /// tile leads to.
    Name(TextField),
    /// What is the account's secret?
    Secret,
}

/// The rectangles the last painted frame put things in.
///
/// Recorded so a later change confined to one of them — a keystroke, a clock
/// tick — can be reported as that rectangle rather than as the whole screen.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Placement {
    panel: Rect,
    field: Rect,
    chooser: Rect,
    chrome: Rect,
    /// The screen the last paint or event placed against.
    screen: Rect,
    /// The scale that paint used.
    scale: Scale,
}

/// One authentication surface: the account chooser, the panel headed with the
/// account name, the masked field the secret is typed into, and the notice
/// under it.
///
/// The surface is modal and total by construction. It offers no cancel, no
/// timeout, and no affordance that concludes it: [`on_event`](Self::on_event)
/// reports [`Outcome::verified`] only when a [`Verifier`] answered
/// [`Verdict::Verified`], so no key, no click, no empty account list, and no
/// cooldown expiring can conclude it without one. Enforcing that *nothing
/// else* on the machine sees those events is the embedder's half of the
/// contract; a surface cannot do it from the inside.
///
/// Nothing here rate-limits or counts attempts, deliberately. The authority
/// behind the [`Verifier`] owns that policy and audits every attempt against
/// the account; a second policy on this side would be a second place to get
/// it wrong, and one that could not slow down an attacker holding the
/// keyboard anyway. [`set_cooldown`](Self::set_cooldown) *presents* the
/// authority's budget; it does not invent one and reads no clock.
pub struct AuthSurface {
    mode: Mode,
    chooser: Option<Chooser>,
    account: String,
    /// The name shown under the disc — an account's display name, which is
    /// not always the login name the [`Verifier`] is asked about.
    heading: String,
    field: TextField,
    notice: String,
    /// Bumped whenever the notice text changes, so an event can tell a
    /// keystroke's damage from a verdict's without copying the line.
    notice_rev: u32,
    cooldown: Duration64,
    chrome: Chrome,
    placement: Cell<Option<Placement>>,
    /// The chooser and the prompt trading places, while one is doing so.
    stage: Option<Stage>,
    /// The question shaking, while a rejected attempt is being answered.
    shake: Option<Shake>,
    /// The black the screen leaves through, once a secret is accepted.
    veil: Option<Veil>,
}

/// How one stage is drawn this frame.
///
/// A settled stage is drawn at full strength with its own disc and no
/// displacement, which is exactly what the fields below say; a stage giving
/// way says so once here rather than threading four arguments through every
/// part of the paint.
#[derive(Copy, Clone)]
struct Draw<'a> {
    /// How much of its own opacity every element takes.
    strength: u8,
    /// The name under the disc.
    heading: &'a str,
    /// The line under the field.
    notice: &'a str,
    /// Whether this stage draws its own disc, or a travelling one carries it.
    disc: bool,
    /// How far the disc, the name, and the pill are displaced sideways.
    offset: i32,
}

/// `rect` moved `by` pixels sideways.
fn shifted(rect: Rect, by: i32) -> Rect {
    Rect::new(
        rect.origin.x.saturating_add(by),
        rect.origin.y,
        rect.width,
        rect.height,
    )
}

impl AuthSurface {
    /// A surface asking for `account`'s secret, focused and ready to type
    /// into. An empty name heads it with [`UNNAMED_ACCOUNT`].
    ///
    /// There is no chooser behind it, so `Escape` leads nowhere: a screen
    /// lock asks about the one account whose session it covers, and stepping
    /// back from that question is not an answer.
    #[must_use]
    pub fn new(account: &str) -> Self {
        let account = if account.is_empty() {
            UNNAMED_ACCOUNT
        } else {
            account
        };
        Self {
            mode: Mode::Secret,
            chooser: None,
            account: account.to_string(),
            heading: account.to_string(),
            field: secret_field(),
            notice: HINT.to_string(),
            notice_rev: 0,
            cooldown: Duration64::ZERO,
            chrome: Chrome::default(),
            placement: Cell::new(None),
            stage: None,
            shake: None,
            veil: None,
        }
    }

    /// A surface starting on the chooser, offering `accounts` and — always
    /// last, and present even when `accounts` is empty — the `Other…` tile
    /// that leads to a typed login name.
    #[must_use]
    pub fn with_accounts(accounts: Vec<AccountTile>) -> Self {
        Self {
            mode: Mode::Chooser,
            chooser: Some(Chooser::new(accounts)),
            account: String::new(),
            heading: String::new(),
            field: secret_field(),
            notice: CHOOSE_HINT.to_string(),
            notice_rev: 0,
            cooldown: Duration64::ZERO,
            chrome: Chrome::default(),
            placement: Cell::new(None),
            stage: None,
            shake: None,
            veil: None,
        }
    }

    /// The line currently shown under the field: the resting hint, the
    /// remaining cooldown, or the answer to the last secret offered.
    #[must_use]
    pub fn notice(&self) -> &str {
        &self.notice
    }

    /// The login name a secret is being asked for, or `None` while the
    /// surface is still asking *which* account.
    #[must_use]
    pub fn selected_account(&self) -> Option<&str> {
        matches!(self.mode, Mode::Secret).then_some(self.account.as_str())
    }

    /// Show the clock, date, and host name on the backdrop.
    ///
    /// The strings are cut to [`MAX_CHROME`] characters and drawn as text and
    /// nothing more. Chrome that is already on screen changes nothing.
    pub fn set_chrome(&mut self, chrome: Chrome) -> Outcome {
        let chrome = chrome.bounded();
        if chrome == self.chrome {
            return Outcome::quiet();
        }
        self.chrome = chrome;
        Outcome::changed(self.placement.get().map(|placed| placed.chrome))
    }

    /// Present the authority's remaining per-account lockout.
    ///
    /// While it is non-zero the surface shows it and refuses to submit, so a
    /// locked-out account cannot even reach the [`Verifier`]. Zero clears it,
    /// as does a negative span, which is not a lockout. The engine invents no
    /// budget and reads no clock: the embedder supplies what remains.
    pub fn set_cooldown(&mut self, remaining: Duration64) -> Outcome {
        let remaining = remaining.max(Duration64::ZERO);
        if remaining == self.cooldown {
            return Outcome::quiet();
        }
        self.cooldown = remaining;
        if self.is_cooling() {
            self.show(cooldown_notice(remaining), ValidationState::Invalid);
        } else {
            self.show(self.resting_hint().to_string(), ValidationState::Valid);
        }
        Outcome::changed(self.placement.get().map(|placed| placed.panel))
    }

    /// Apply one input event — pointer or key, in the surface's own
    /// coordinate space.
    ///
    /// On the chooser, `Tab` and the arrow keys move the keyboard between
    /// tiles (wrapping at both ends), `Return` picks the focused one, and the
    /// pointer picks the tile it is released over. On a field, keys edit and
    /// `Enter` offers what was typed; the pointer places the caret and
    /// selects within the field, and reaches nothing else. `Escape` steps
    /// back to the chooser when there is one.
    ///
    /// The secret is erased before this returns on every path out of an
    /// offer — accepted, refused, unanswerable, or refused for a cooldown —
    /// and on every step back to the chooser, so no later branch can be the
    /// one that forgot.
    pub fn on_event(&mut self, event: &InputEvent, ctx: &mut EventContext<'_>) -> Outcome {
        // Once the screen has begun leaving, the answer is already given and
        // nothing may take it back.
        if self.session_fade_begun() {
            return Outcome::quiet();
        }
        self.place(ctx.screen, ctx.scale, ctx.theme);
        match self.mode {
            Mode::Chooser => self.on_chooser_event(event, ctx),
            Mode::Name(_) => self.on_name_event(event, ctx),
            Mode::Secret => self.on_secret_event(event, ctx),
        }
    }

    /// Paint the whole frame: the backdrop, the chrome, and then the chooser
    /// or the panel and its field.
    ///
    /// `screen` is the rectangle the returned surface covers, in its own
    /// coordinate space. `None` means no frame could be produced — a screen
    /// with no pixels, or a surface that could never be allocated — so a
    /// caller that must cover the screen fails closed instead of presenting
    /// something that covers nothing.
    #[must_use]
    pub fn render(
        &self,
        screen: Rect,
        scale: Scale,
        theme: &Theme,
        backdrop: Backdrop<'_>,
    ) -> Option<Surface> {
        if screen.width == 0 || screen.height == 0 {
            return None;
        }
        let mut surface = Surface::new(screen.width, screen.height)?;
        paint_backdrop(&mut surface, theme, backdrop);
        self.paint_chrome(&mut surface, screen, scale, theme);

        let offset = self.shake_offset(screen, scale, theme);
        match self.stage {
            Some(stage) => self.paint_stages(&mut surface, stage, screen, scale, theme, offset),
            None => self.paint_settled(&mut surface, screen, scale, theme, offset),
        }
        if let Some(veil) = self.veil {
            let (w, h) = (surface.width(), surface.height());
            surface.fill_round_rect(
                0,
                0,
                w,
                h,
                0,
                Color::from(at_strength(VEIL, veil.strength())),
            );
        }
        self.place(screen, scale, theme);
        Some(surface)
    }

    /// Where the secret field — or the login-name field that precedes it —
    /// sits on `screen`: the pill at the top of the prompt block.
    ///
    /// The one definition of the field's placement, so the paint and the
    /// pointer hit test resolve the same rectangle rather than each deriving
    /// its own. Its height is the theme's control height, which is what the
    /// shared field lays its own row out at, clamped to the block so the pill
    /// can never be taller than the region reported as damaged.
    #[must_use]
    pub fn field_rect(&self, screen: Rect, scale: Scale, theme: &Theme) -> Rect {
        let block = panel_rect(screen, scale);
        let w = scale.scale_length(FIELD_WIDTH).min(block.width);
        let h = scale
            .scale_length(theme.metrics().control_height)
            .max(1)
            .min(block.height);
        Rect::new(
            centre_on(block.origin.x, block.width, w),
            block.origin.y,
            w,
            h,
        )
    }

    /// Step every running animation and report the damage.
    ///
    /// A selection fade damages the tiles whose mark strength moved; a
    /// rejected attempt's shake damages the band it displaces; a stage
    /// transition and the session fade both cover the screen. A round that
    /// moved nothing repaints nothing.
    pub fn advance(&mut self, now_ns: u64) -> Outcome {
        let placed = self.placement.get();
        let mut changed = self.advance_selection(now_ns, placed);

        if let Some(mut stage) = self.stage {
            let moved = stage.advance(now_ns);
            self.stage = (!stage.finished(now_ns)).then_some(stage);
            if moved {
                changed = changed.merged(Changed::Whole);
            }
        }
        // Nothing draws the chooser once the prompt has it, so a mark still
        // crossing there is settled rather than left asking for frames.
        if self.stage.is_none() && !matches!(self.mode, Mode::Chooser) {
            if let Some(chooser) = self.chooser.as_mut() {
                chooser.settle_selection();
            }
        }
        if let Some(mut shake) = self.shake {
            let moved = shake.advance(now_ns);
            self.shake = (!shake.finished(now_ns)).then_some(shake);
            if moved {
                changed = changed.merged(match placed {
                    Some(placed) => Changed::Region(shake_band(placed)),
                    None => Changed::Whole,
                });
            }
        }
        if let Some(mut veil) = self.veil {
            if veil.advance(now_ns) {
                changed = changed.merged(Changed::Whole);
            }
            self.veil = Some(veil);
        }

        if !changed.moved() {
            return Outcome::quiet();
        }
        Outcome::changed(changed.damage())
    }

    /// Nanoseconds until the next animation frame, or `None` when nothing is
    /// animating — including under reduced motion, which leaves every
    /// duration at zero and so arms no timer at all.
    #[must_use]
    pub fn motion_due(&self, now_ns: u64) -> Option<u64> {
        let mut due = self.selection_due(now_ns);
        if let Some(stage) = self.stage {
            due = sooner(due, stage.next_frame_in(now_ns));
        }
        if let Some(shake) = self.shake {
            due = sooner(due, shake.next_frame_in(now_ns));
        }
        if let Some(veil) = self.veil {
            due = sooner(due, veil.next_frame_in(now_ns));
        }
        due
    }

    /// Begin the fade the screen leaves through, once a secret has been
    /// accepted.
    ///
    /// The black runs to full over the theme's session-fade duration, and the
    /// surface stops answering input the moment it begins: the decision is
    /// made, and a keystroke must not take it back. An owner presents frames
    /// while [`motion_due`](Self::motion_due) asks for them and leaves once
    /// [`session_fade_finished`](Self::session_fade_finished) says so — which
    /// a reduced-motion theme says immediately, with no frame to present.
    ///
    /// Beginning a fade already begun changes nothing.
    pub fn begin_session_fade(&mut self, now_ns: u64, theme: &Theme) -> Outcome {
        if self.veil.is_some() {
            return Outcome::quiet();
        }
        let duration = theme.motion().duration(MotionInteraction::SessionFade);
        self.veil = Some(Veil::start(now_ns, duration));
        Outcome::changed(None)
    }

    /// Whether the screen has begun leaving, from the first veiled frame on.
    ///
    /// An owner that draws a pointer over this surface stops drawing it from
    /// here: a pointer is something to point *with*, and the screen has
    /// stopped answering input, so an arrow left over the black points at
    /// nothing. It leaves with the screen it belonged to.
    #[must_use]
    pub const fn session_fade_begun(&self) -> bool {
        self.veil.is_some()
    }

    /// Whether the screen has finished going black, so its owner may leave.
    ///
    /// `false` until [`begin_session_fade`](Self::begin_session_fade) has been
    /// called: a screen that never began leaving has not finished doing so.
    #[must_use]
    pub fn session_fade_finished(&self) -> bool {
        self.veil.is_some_and(Veil::finished)
    }

    /// Step the chooser's selection cross-fade, if anything is drawing it.
    fn advance_selection(&mut self, now_ns: u64, placed: Option<Placement>) -> Changed {
        if !matches!(self.mode, Mode::Chooser) && self.stage.is_none() {
            return Changed::Nothing;
        }
        let Some(chooser) = self.chooser.as_mut() else {
            return Changed::Nothing;
        };
        // Capture the tiles before the step settles them away.
        let (slots, count) = chooser.animating_slots();
        if !chooser.advance(now_ns) {
            return Changed::Nothing;
        }
        let Some(placed) = placed else {
            return Changed::Whole;
        };
        let tiles = if count > 0 {
            chooser.tile_bounds_of(&slots[..count], placed.screen, placed.scale)
        } else {
            chooser.fade_damage(placed.screen, placed.scale)
        };
        Changed::Region(tiles.unwrap_or(placed.chooser))
    }

    /// When the chooser's selection cross-fade next needs a frame.
    fn selection_due(&self, now_ns: u64) -> Option<u64> {
        if !matches!(self.mode, Mode::Chooser) && self.stage.is_none() {
            return None;
        }
        self.chooser.as_ref()?.next_frame_in(now_ns)
    }

    /// One event on the chooser.
    fn on_chooser_event(&mut self, event: &InputEvent, ctx: &mut EventContext<'_>) -> Outcome {
        let Some(chooser) = self.chooser.as_mut() else {
            return Outcome::quiet();
        };
        let duration_ms = ctx
            .theme
            .motion()
            .duration(MotionInteraction::SelectionChange);
        let (chosen, stirred) = match event {
            InputEvent::KeyPressed { key, modifiers } => match key {
                Key::Named(NamedKey::Tab) if modifiers.shift => (
                    None,
                    chooser.move_focus(Step::Previous, ctx.now_ns, duration_ms),
                ),
                Key::Named(NamedKey::Tab | NamedKey::Right | NamedKey::Down) => (
                    None,
                    chooser.move_focus(Step::Next, ctx.now_ns, duration_ms),
                ),
                Key::Named(NamedKey::Left | NamedKey::Up) => (
                    None,
                    chooser.move_focus(Step::Previous, ctx.now_ns, duration_ms),
                ),
                Key::Named(NamedKey::Enter) => (Some(chooser.focus()), false),
                _ => (None, false),
            },
            _ => chooser.on_pointer(event, ctx.screen, ctx.scale, ctx.now_ns, duration_ms),
        };
        match chosen {
            Some(slot) => self.choose(slot, ctx.now_ns, stage_ms(ctx.theme)),
            None if stirred => {
                let damage = match self.placement.get() {
                    Some(placed) => chooser
                        .focus_move_damage(placed.screen, placed.scale)
                        .or(Some(placed.chooser)),
                    None => chooser
                        .focus_move_damage(ctx.screen, ctx.scale)
                        .or(Some(chooser.bounds(ctx.screen, ctx.scale))),
                };
                Outcome::changed(damage)
            }
            None => Outcome::quiet(),
        }
    }

    /// One event on the typed-login-name field.
    fn on_name_event(&mut self, event: &InputEvent, ctx: &mut EventContext<'_>) -> Outcome {
        if is_escape(event) {
            return self.back_to_chooser(ctx.now_ns, stage_ms(ctx.theme));
        }
        let bounds = self.field_rect(ctx.screen, ctx.scale, ctx.theme);
        let (scale, theme) = (ctx.scale, ctx.theme);
        let Mode::Name(name) = &mut self.mode else {
            return Outcome::quiet();
        };
        let (action, redraw) = if let InputEvent::KeyPressed { key, modifiers } = event {
            (name.on_key(*key, *modifiers), true)
        } else {
            let was = name.state();
            let action = name.on_pointer(event, bounds, scale, theme);
            let now = name.state();
            let dragging = now.pointer == PointerState::Pressed;
            (action, action.is_some() || now != was || dragging)
        };
        let typed = (action == Some(TextAction::Submitted)).then(|| name.text().trim().to_string());
        match typed {
            // A name is what the next question is asked about, so an empty
            // one does not move on.
            Some(typed) if typed.is_empty() => {
                self.show(NAME_REQUIRED.to_string(), ValidationState::Invalid);
                Outcome::changed(self.placement.get().map(|placed| placed.panel))
            }
            Some(typed) => {
                let shown = typed.clone();
                self.ask_for(&typed, &shown)
            }
            None if redraw => Outcome::changed(self.placement.get().map(|placed| placed.field)),
            None => Outcome::quiet(),
        }
    }

    /// One event on the secret field.
    fn on_secret_event(&mut self, event: &InputEvent, ctx: &mut EventContext<'_>) -> Outcome {
        if is_escape(event) && self.chooser.is_some() {
            return self.back_to_chooser(ctx.now_ns, stage_ms(ctx.theme));
        }
        let said = self.notice_rev;
        let (submitted, redraw) = if let InputEvent::KeyPressed { key, modifiers } = event {
            // Typing asks the question again, so a verdict on the previous
            // secret stops standing over the new one. A live cooldown is not
            // a verdict and stands until it is cleared.
            if !self.is_cooling() {
                self.show(HINT.to_string(), ValidationState::Valid);
            }
            let action = self.field.on_key(*key, *modifiers);
            // A key can move the caret without reporting an edit, and arrives
            // at human typing rate either way, so every one is worth a frame.
            (matches!(action, Some(TextAction::Submitted)), true)
        } else {
            let bounds = self.field_rect(ctx.screen, ctx.scale, ctx.theme);
            let was = self.field.state();
            let action = self.field.on_pointer(event, bounds, ctx.scale, ctx.theme);
            let now = self.field.state();
            // Pointer motion is the one event that streams. Repainting the
            // whole screen for each one would rebuild a screen-sized surface
            // per sample for no visible difference, so a frame is drawn only
            // when the field's rendering can actually have changed: it
            // edited, its state moved (a hover arriving or leaving), or a
            // button is down and the motion is extending a selection.
            let dragging = now.pointer == PointerState::Pressed;
            (
                matches!(action, Some(TextAction::Submitted)),
                action.is_some() || now != was || dragging,
            )
        };
        let rejected_ms = ctx
            .theme
            .motion()
            .duration(MotionInteraction::AttemptRejected);
        if submitted && self.offer(&mut *ctx.verifier, ctx.now_ns, rejected_ms) {
            return Outcome {
                redraw: true,
                verified: true,
                damage: None,
            };
        }
        if !redraw {
            return Outcome::quiet();
        }
        Outcome::changed(self.placement.get().map(|placed| {
            if self.notice_rev == said {
                placed.field
            } else {
                placed.panel
            }
        }))
    }

    /// Act on the chooser tile at `slot`: an account leads straight to its
    /// secret, the trailing `Other…` tile to a typed login name. The tile's
    /// disc travels to the prompt's place as it goes.
    fn choose(&mut self, slot: usize, now_ns: u64, duration_ms: u16) -> Outcome {
        self.turn(slot, Toward::Prompt, now_ns, duration_ms);
        let picked = self
            .chooser
            .as_ref()
            .and_then(|chooser| chooser.account(slot))
            .map(|account| {
                (
                    account.login_name().to_string(),
                    account.display_name().to_string(),
                )
            });
        match picked {
            Some((login, shown)) => self.ask_for(&login, &shown),
            None => self.ask_for_name(),
        }
    }

    /// Ask for `login`'s secret, on a field with nothing in it, under
    /// `heading` — the account's display name, which the authority is never
    /// asked about.
    fn ask_for(&mut self, login: &str, heading: &str) -> Outcome {
        self.leave();
        self.account = login.to_string();
        self.heading = heading.to_string();
        self.mode = Mode::Secret;
        self.show(HINT.to_string(), ValidationState::Valid);
        Outcome::changed(None)
    }

    /// Step forward to the typed-login-name field.
    fn ask_for_name(&mut self) -> Outcome {
        self.leave();
        self.account = String::new();
        self.heading = String::new();
        self.mode = Mode::Name(name_field());
        self.show(NAME_HINT.to_string(), ValidationState::Valid);
        Outcome::changed(None)
    }

    /// Step back to the chooser, taking whatever was typed with it. The
    /// prompt's disc travels back to the tile it came from.
    fn back_to_chooser(&mut self, now_ns: u64, duration_ms: u16) -> Outcome {
        let Some(chooser) = self.chooser.as_ref() else {
            return Outcome::quiet();
        };
        self.turn(chooser.focus(), Toward::Chooser, now_ns, duration_ms);
        self.leave();
        self.account = String::new();
        self.mode = Mode::Chooser;
        self.show(CHOOSE_HINT.to_string(), ValidationState::Valid);
        Outcome::changed(None)
    }

    /// Drop everything that belonged to the account being left: whatever was
    /// typed into either field, and the lockout the authority reported for
    /// that account, which says nothing about the next one.
    fn leave(&mut self) {
        self.field.set_text("");
        if let Mode::Name(name) = &mut self.mode {
            name.set_text("");
        }
        self.cooldown = Duration64::ZERO;
    }

    /// Begin — or turn round — the transition carrying `slot`'s disc.
    ///
    /// An in-flight travel of the same disc is reversed from where it had
    /// reached rather than restarted, so a person who changes their mind
    /// half-way sees the disc turn round instead of jump.
    fn turn(&mut self, slot: usize, toward: Toward, now_ns: u64, duration_ms: u16) {
        self.stage = match self.stage {
            Some(running) if running.slot() == slot && running.toward() != toward => {
                running.reverse(now_ns, duration_ms)
            }
            _ => Stage::start(slot, toward, now_ns, duration_ms),
        };
    }

    /// Offer the typed secret to `verifier` and record what came back.
    ///
    /// Returns whether the account was verified. The secret is erased before
    /// this returns, on every path — including the one where a live cooldown
    /// means it was never offered at all.
    ///
    /// A rejected attempt — refused outright, or refused for a standing
    /// lockout — shakes the question as well as saying so in the notice. An
    /// authority that could not be reached refused nothing, so nothing
    /// shakes.
    fn offer(&mut self, verifier: &mut dyn Verifier, now_ns: u64, duration_ms: u16) -> bool {
        if self.is_cooling() {
            self.field.set_text("");
            self.show(cooldown_notice(self.cooldown), ValidationState::Invalid);
            self.shake = Shake::start(now_ns, duration_ms);
            return false;
        }
        let verdict = verifier.verify(&self.account, self.field.text());
        self.field.set_text("");
        match verdict {
            Verdict::Verified => true,
            Verdict::Refused => {
                self.show(REFUSED.to_string(), ValidationState::Invalid);
                self.shake = Shake::start(now_ns, duration_ms);
                false
            }
            Verdict::Unreachable => {
                self.show(UNREACHABLE.to_string(), ValidationState::Invalid);
                false
            }
        }
    }

    /// Whether the authority's lockout still has time left on it.
    fn is_cooling(&self) -> bool {
        self.cooldown > Duration64::ZERO
    }

    /// The line this mode shows when there is nothing else to say.
    fn resting_hint(&self) -> &'static str {
        match self.mode {
            Mode::Chooser => CHOOSE_HINT,
            Mode::Name(_) => NAME_HINT,
            Mode::Secret => HINT,
        }
    }

    /// Show `notice` under the field at `validation`, so the answer is both
    /// readable and stated on the pill's own edge rather than only in prose.
    fn show(&mut self, notice: String, validation: ValidationState) {
        if notice != self.notice {
            self.notice_rev = self.notice_rev.wrapping_add(1);
        }
        self.field
            .set_state(self.field.state().with_validation(validation));
        if let Mode::Name(name) = &mut self.mode {
            name.set_state(name.state().with_validation(validation));
        }
        self.notice = notice;
    }

    /// Record where this geometry puts the surface's parts, so a later change
    /// confined to one of them reports that rectangle rather than the screen.
    fn place(&self, screen: Rect, scale: Scale, theme: &Theme) {
        let panel = panel_rect(screen, scale);
        self.placement.set(Some(Placement {
            panel,
            field: self.field_rect(screen, scale, theme),
            chooser: self
                .chooser
                .as_ref()
                .map_or(panel, |chooser| chooser.bounds(screen, scale)),
            chrome: chrome_band(screen, scale),
            screen,
            scale,
        }));
    }

    /// Paint whichever body is up, at full strength.
    fn paint_settled(
        &self,
        surface: &mut Surface,
        screen: Rect,
        scale: Scale,
        theme: &Theme,
        offset: i32,
    ) {
        let draw = Draw {
            strength: u8::MAX,
            heading: self.shown_heading(),
            notice: &self.notice,
            disc: true,
            offset,
        };
        match &self.mode {
            Mode::Chooser => {
                self.paint_chooser(surface, screen, scale, theme, draw.strength, draw.notice);
            }
            Mode::Name(name) => self.paint_prompt(surface, name, screen, scale, theme, draw),
            Mode::Secret => self.paint_prompt(surface, &self.field, screen, scale, theme, draw),
        }
    }

    /// Paint both stages of a transition and the disc that travels between
    /// them.
    ///
    /// Each stage is drawn once, at a strength, into the frame already being
    /// painted — there is no second screen to cross-fade against. Only the
    /// destination stage shows the live notice; the stage being left shows
    /// its own resting line, because a transition is not the moment to read
    /// a verdict.
    ///
    /// The order is the same whichever way the transition runs — chooser,
    /// then prompt, then the disc over both. Ordering by which stage is
    /// arriving would re-order the two where they overlap, and a travel
    /// turned round half-way would pop.
    fn paint_stages(
        &self,
        surface: &mut Surface,
        stage: Stage,
        screen: Rect,
        scale: Scale,
        theme: &Theme,
        offset: i32,
    ) {
        let prompt_strength = stage.prompt_strength();
        let field = match &self.mode {
            Mode::Name(name) => name,
            _ => &self.field,
        };
        self.paint_chooser(
            surface,
            screen,
            scale,
            theme,
            u8::MAX - prompt_strength,
            self.stage_notice(stage, Toward::Chooser),
        );
        self.paint_prompt(
            surface,
            field,
            screen,
            scale,
            theme,
            Draw {
                strength: prompt_strength,
                heading: self.stage_heading(stage.slot()),
                notice: self.stage_notice(stage, Toward::Prompt),
                disc: false,
                offset,
            },
        );
        self.paint_travelling_disc(surface, stage, screen, scale, theme, offset);
    }

    /// Paint the tile grid and its one hint line at `strength`.
    fn paint_chooser(
        &self,
        surface: &mut Surface,
        screen: Rect,
        scale: Scale,
        theme: &Theme,
        strength: u8,
        notice: &str,
    ) {
        let Some(chooser) = self.chooser.as_ref() else {
            return;
        };
        if strength == 0 {
            return;
        }
        chooser.render(surface, screen, scale, theme, strength);
        draw_centred(
            surface,
            chooser.hint_rect(screen, scale),
            notice,
            BitmapFont::for_role(theme.fonts(), TextRole::Body, scale),
            at_strength(theme.palette().on_surface_muted, strength),
        );
    }

    /// Paint the disc on its way between the tile it was picked from and the
    /// prompt's own place, growing as it goes.
    ///
    /// It carries the prompt's strength: it lifts off the tile whose own disc
    /// is dissolving and settles as the prompt's, so exactly one disc is on
    /// screen at either end of the travel.
    fn paint_travelling_disc(
        &self,
        surface: &mut Surface,
        stage: Stage,
        screen: Rect,
        scale: Scale,
        theme: &Theme,
        offset: i32,
    ) {
        let strength = stage.prompt_strength();
        let Some(chooser) = self.chooser.as_ref() else {
            return;
        };
        if strength == 0 {
            return;
        }
        let Some(from) = chooser.tile_disc_rect(stage.slot(), screen, scale, theme) else {
            return;
        };
        let rect = shifted(
            between_rects(from, Prompt::new(screen, scale).disc, strength),
            offset,
        );
        let (mark, fill, ink) = chooser.slot_disc(stage.slot(), theme);
        let font = travelling_font(theme, scale, strength);
        let Some(mut disc) = monogram_disc(mark, rect.width, font, (fill, ink)) else {
            return;
        };
        fade(&mut disc, strength);
        surface.blit(rect.origin.x, rect.origin.y, &disc);
    }

    /// The name a transition's prompt stage shows, taken from the tile the
    /// disc belongs to so it is right whichever way the transition runs.
    fn stage_heading(&self, slot: usize) -> &str {
        match self.chooser.as_ref().and_then(|it| it.account(slot)) {
            Some(account) => account.display_name(),
            None => NAME_HEADING,
        }
    }

    /// The line the `side` stage shows during `stage`: the live notice when
    /// it is the destination, its own resting line when it is being left.
    fn stage_notice(&self, stage: Stage, side: Toward) -> &str {
        if stage.toward() == side {
            return &self.notice;
        }
        match side {
            Toward::Chooser => CHOOSE_HINT,
            Toward::Prompt if self.stage_heading(stage.slot()) == NAME_HEADING => NAME_HINT,
            Toward::Prompt => HINT,
        }
    }

    /// Paint the prompt: the account's disc, its name, the pill, and the one
    /// or two lines under them.
    ///
    /// The disc, the name, and the pill take the shake's displacement — they
    /// are the question that was refused. The lines under them do not: the
    /// answer has to stay still to be read.
    fn paint_prompt(
        &self,
        surface: &mut Surface,
        field: &TextField,
        screen: Rect,
        scale: Scale,
        theme: &Theme,
        draw: Draw<'_>,
    ) {
        if draw.strength == 0 {
            return;
        }
        let palette = theme.palette();
        let prompt = Prompt::new(screen, scale);
        if draw.disc {
            if let Some(disc) = self.disc(prompt.disc.width, scale, theme, draw.strength) {
                let at = shifted(prompt.disc, draw.offset);
                surface.blit(at.origin.x, at.origin.y, &disc);
            }
        }
        draw_centred(
            surface,
            shifted(prompt.name, draw.offset),
            draw.heading,
            BitmapFont::for_role(theme.fonts(), TextRole::Heading, scale),
            at_strength(palette.on_surface, draw.strength),
        );

        let pill = self.field_rect(screen, scale, theme);
        paint_pill(
            surface,
            field,
            shifted(pill, draw.offset),
            scale,
            theme,
            draw.strength,
        );

        let caption = BitmapFont::for_role(theme.fonts(), TextRole::Caption, scale);
        let Some(notice) = notice_band(prompt.block, pill, scale) else {
            return;
        };
        let ink = if field.state().validation == ValidationState::Valid {
            palette.on_surface_muted
        } else {
            palette.danger
        };
        draw_centred(
            surface,
            notice,
            draw.notice,
            caption,
            at_strength(ink, draw.strength),
        );

        if self.chooser.is_none() {
            return;
        }
        if let Some(back) = back_band(prompt.block, notice, scale) {
            draw_centred(
                surface,
                back,
                BACK_HINT,
                caption,
                at_strength(palette.on_surface_muted, draw.strength),
            );
        }
    }

    /// The name drawn under the disc: the chosen account's, or the heading
    /// over the typed-login-name field.
    fn shown_heading(&self) -> &str {
        match self.mode {
            Mode::Name(_) => NAME_HEADING,
            _ => &self.heading,
        }
    }

    /// The prompt's `side`×`side` disc at `strength`: the chosen account's
    /// mark in the accent, or the chooser's own trailing mark while a login
    /// name is still being typed.
    fn disc(&self, side: u32, scale: Scale, theme: &Theme, strength: u8) -> Option<Surface> {
        let palette = theme.palette();
        let (mark, fill, ink) = match self.mode {
            Mode::Name(_) => (OTHER_MONOGRAM, palette.surface_raised, palette.on_surface),
            _ => (
                monogram_of(&self.heading),
                palette.accent,
                palette.on_accent,
            ),
        };
        let mut disc = monogram_disc(
            mark,
            side,
            BitmapFont::for_role(theme.fonts(), TextRole::Display, scale),
            (fill, ink),
        )?;
        fade(&mut disc, strength);
        Some(disc)
    }

    /// How far the question is displaced this frame, clamped to the room the
    /// pill has either side of it.
    fn shake_offset(&self, screen: Rect, scale: Scale, theme: &Theme) -> i32 {
        let Some(shake) = self.shake else {
            return 0;
        };
        let pill = self.field_rect(screen, scale, theme);
        let left = u32::try_from(pill.origin.x.saturating_sub(screen.origin.x)).unwrap_or(0);
        let right = u32::try_from(screen.right().saturating_sub(pill.right())).unwrap_or(0);
        shake.offset(scale, (left, right))
    }

    /// Paint the clock, the date, and the host name at the top of the column.
    fn paint_chrome(&self, surface: &mut Surface, screen: Rect, scale: Scale, theme: &Theme) {
        let band = chrome_band(screen, scale);
        if self.chrome.is_empty() || band.height == 0 {
            return;
        }
        let palette = theme.palette();
        let [clock, date, host] = chrome_bands(band, scale);
        for (band, text, role, ink) in [
            (
                clock,
                &self.chrome.clock,
                TextRole::Display,
                palette.on_surface,
            ),
            (date, &self.chrome.date, TextRole::Body, palette.on_surface),
            (
                host,
                &self.chrome.host,
                TextRole::Caption,
                palette.on_surface_muted,
            ),
        ] {
            let font = BitmapFont::for_role(theme.fonts(), role, scale);
            draw_centred(surface, band, text, font, ink);
        }
    }
}

/// The block the secret field and the lines under it occupy on `screen`:
/// centred horizontally, under the account's disc and name.
///
/// This is the region whose legibility the scrim is chosen for, so it is
/// wider than the field itself — the notice under the pill is prose, and a
/// block only as wide as the pill would cut it short. A screen smaller than
/// the block gets the whole screen: the prompt is still usable, and a surface
/// that refused to draw on a small display would be one that did not ask.
#[must_use]
pub fn panel_rect(screen: Rect, scale: Scale) -> Rect {
    Prompt::new(screen, scale).block
}

/// How long one stage transition runs in `theme`.
fn stage_ms(theme: &Theme) -> u16 {
    theme.motion().duration(MotionInteraction::StageTransition)
}

/// The band a rejected attempt's shake displaces: the disc, the name, and
/// the pill.
///
/// Full width, because the name is centred across the screen rather than in
/// the block, so every position the reach can take is already inside it.
fn shake_band(placed: Placement) -> Rect {
    let top = Prompt::new(placed.screen, placed.scale).disc.origin.y;
    let height = u32::try_from(placed.field.bottom().saturating_sub(top)).unwrap_or(0);
    Rect::new(placed.screen.origin.x, top, placed.screen.width, height)
}

/// Whether `event` is the Escape key going down.
fn is_escape(event: &InputEvent) -> bool {
    matches!(
        event,
        InputEvent::KeyPressed {
            key: Key::Named(NamedKey::Escape),
            ..
        }
    )
}

/// The line a live cooldown shows, rounded up so a lockout with any time left
/// on it never reads as over.
fn cooldown_notice(remaining: Duration64) -> String {
    let seconds = remaining
        .secs()
        .saturating_add(i64::from(remaining.subsec_nanos() > 0));
    format!("Too many attempts — try again in {seconds}s")
}

/// Fill `surface` with what lies behind the column.
fn paint_backdrop(surface: &mut Surface, theme: &Theme, backdrop: Backdrop<'_>) {
    let desktop = Color::from(theme.palette().desktop);
    surface.fill(desktop);
    let (w, h) = (surface.width(), surface.height());
    if let Backdrop::Wallpaper { image, scrim } = backdrop {
        surface.blit(0, 0, image);
        // Through the compositing rounded fill at zero radius, because a plain
        // rectangle fill overwrites rather than blends and would hide the
        // picture outright.
        surface.fill_round_rect(
            0,
            0,
            w,
            h,
            0,
            Color::rgba(desktop.r, desktop.g, desktop.b, scrim),
        );
    }
    // No blur is reachable from here — that lives in the compositor — so the
    // ends of the screen carry more of the desktop colour instead, where the
    // chrome and the prompt sit. It is the desktop colour itself, so over the
    // flat backdrop the wash composites to exactly what is already there and
    // only a picture ever sees it.
    let band = (h / WASH_BANDS).max(1);
    let wash = Color::rgba(desktop.r, desktop.g, desktop.b, WASH_ALPHA);
    let clear = Color::rgba(desktop.r, desktop.g, desktop.b, 0);
    surface.fill_vertical_gradient(0, 0, w, band, wash, clear);
    surface.fill_vertical_gradient(0, h.saturating_sub(band), w, band, clear, wash);
}

/// Paint one field as the prompt's pill.
///
/// The shared field draws its own plate at the theme's control radius, which
/// is nowhere near a stadium, so the row is rendered off-screen, confined to
/// a stadium inset by the pill's edge, and laid over a stadium of that edge
/// colour. What shows around it is the pill's one edge, in place of the
/// plate's rim, its focus gap, and its focus ring, all three of which the
/// mask takes away — a square ring inside a round field would be the giveaway
/// that the shape was faked. The field is always focused here, so that edge
/// is its focus indication too, and it takes the danger colour on a refusal
/// exactly as the plate's own rim would have.
fn paint_pill(
    surface: &mut Surface,
    field: &TextField,
    rect: Rect,
    scale: Scale,
    theme: &Theme,
    strength: u8,
) {
    let (Ok(x), Ok(y)) = (u32::try_from(rect.origin.x), u32::try_from(rect.origin.y)) else {
        return;
    };
    let (w, h) = (rect.width, rect.height);
    if w == 0 || h == 0 {
        return;
    }
    let palette = theme.palette();
    let edge = if field.state().validation == ValidationState::Valid {
        palette.rim_active
    } else {
        palette.danger
    };
    surface.fill_round_rect(x, y, w, h, h / 2, Color::from(at_strength(edge, strength)));

    let Some(mut row) = Surface::new(w, h) else {
        return;
    };
    field.render(&mut row, Rect::new(0, 0, w, h), scale, theme);
    let inset = pill_edge(theme, scale).min(w / 2).min(h / 2);
    if field.text().is_empty() {
        paint_submit(&mut row, inset, theme);
    }
    row.mask_to_round_rect(
        inset,
        inset,
        w - 2 * inset,
        h - 2 * inset,
        (h - 2 * inset) / 2,
    );
    fade(&mut row, strength);
    surface.blit(rect.origin.x, rect.origin.y, &row);
}

/// The pill's edge in physical pixels, matching the rim the shared plate
/// would have drawn so the two are the same weight under any theme.
fn pill_edge(theme: &Theme, scale: Scale) -> u32 {
    let rim = scale
        .scale_length(theme.metrics().border_thickness)
        .max(1)
        .saturating_mul(if theme.contrast() == Contrast::Normal {
            1
        } else {
            2
        });
    rim.saturating_mul(PILL_EDGE_RIMS)
}

/// Draw the trailing "and then press Enter" mark inside the pill.
///
/// Only while the field is empty: once there is something to see, the beads
/// scroll to the trailing edge and a mark there would sit under them.
fn paint_submit(row: &mut Surface, inset: u32, theme: &Theme) {
    let (w, h) = (row.width(), row.height());
    let side = h / 2;
    let margin = inset.saturating_add(side / 2);
    if side == 0 || w <= margin.saturating_add(side) {
        return;
    }
    let ink = Color::from(theme.palette().on_surface_muted);
    let Some(mark) = builtin_icon(IconKind::NavForward, ink).rasterise(side) else {
        return;
    };
    row.blit(
        i32::try_from(w - margin - side).unwrap_or(0),
        i32::try_from((h - side) / 2).unwrap_or(0),
        &mark,
    );
}

/// The masked secret field: bounded, pre-reserved, focused from the moment
/// the surface comes up so the user can simply start typing.
fn secret_field() -> TextField {
    let mut field = TextField::new()
        .secret(MAX_PASSWORD)
        .with_placeholder("Password");
    field.set_focused(true);
    field
}

/// The typed-login-name field the `Other…` tile leads to.
fn name_field() -> TextField {
    let mut field = TextField::new()
        .with_max_len(MAX_LOGIN_NAME)
        .with_placeholder("Login name");
    field.set_focused(true);
    field
}
