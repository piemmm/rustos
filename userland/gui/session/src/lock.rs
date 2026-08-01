//! The desktop session's **screen lock**: a full-screen password prompt that
//! holds the display and every input event until the signed-in user proves
//! they are still the one at the keyboard.
//!
//! Locking is the one way out of a session that keeps the session. Everything
//! carries on running behind the lock — a build finishes, a download
//! completes, an editor keeps its unsaved buffer — but nothing on screen is
//! legible and no keystroke or click reaches any of it.
//!
//! # What makes it a lock rather than a window
//!
//! A modal window is not a lock: a window can be moved, lowered, or simply
//! clicked past. The lock rests on three properties instead, each of which
//! must hold for it to be worth anything:
//!
//! * **It covers the screen.** The surface is the compositor's full extent
//!   and fully opaque, so no pixel of the session shows through. Someone
//!   walking past a locked machine learns nothing about what is on it.
//! * **It takes every event.** While locked, the embedder drains the pointer
//!   and keyboard into [`handle`](ScreenLock::handle) and routes *nothing*
//!   onward — not to the window manager, not to the taskbar, not to a served
//!   application. That is the embedder's contract, stated on
//!   [`is_locked`](ScreenLock::is_locked); a window cannot enforce it from
//!   the inside.
//! * **It stays on top.** [`keep_topmost`](ScreenLock::keep_topmost) raises
//!   it before every composite, so an application that opens or raises a
//!   window behind the lock cannot surface above it.
//!
//! # Who decides it may open again
//!
//! The lock holds no authority to authenticate anybody. It collects a
//! password and hands it to the per-console elevation broker — the login
//! supervisor that started this session — which attests the caller's identity
//! from the kernel, checks the password against *that* uid, and audits the
//! decision. The lock believes only a `Verified` reply. A refusal, a
//! transport failure, a broker that is not there, and a reply it cannot parse
//! are all the same answer: still locked.
//!
//! Nothing here rate-limits or counts attempts, deliberately. The broker owns
//! the authentication policy and audits every attempt against the account;
//! a second policy on this side would be a second place to get it wrong, and
//! one that could not slow down an attacker holding the keyboard anyway.
//!
//! # The password
//!
//! The typed password lives in exactly one place — the masked field's own
//! bounded, pre-reserved buffer — and is erased on every path that leaves the
//! prompt: a successful unlock, a refusal, an unreachable broker, and an
//! abandoned lock alike. The field reserves that buffer once, so typing can
//! never reallocate and strand a copy of the password in a freed block, it
//! draws beads rather than characters, and it redacts itself in `Debug`.

use alloc::string::ToString;

use tairix_abi::Errno;
use tairix_controls::{Panel, PointerState, TextAction, TextField, ValidationState};
use tairix_font::BitmapFont;
use tairix_geometry::Scale;
use tairix_raster::Color;
use tairix_theme::{TextRole, Theme};
use tairix_wm::{Compositor, InputEvent, Point, Rect, Surface, WindowId};

use crate::shell::DesktopShell;

/// Longest password the prompt will hold, in characters.
///
/// The same bound the text login prompt reads a line at, and far below what
/// the elevation wire format accepts. It is a fail-closed memory bound, not
/// a policy on what a password may be: it exists so the field can reserve
/// its buffer once and never grow it, which is what keeps a copy of the
/// secret out of a freed heap block.
pub const MAX_PASSWORD: usize = 256;

/// The prompt panel's width in pixels at the reference density.
pub const PANEL_WIDTH: u32 = 420;

/// The prompt panel's height in pixels at the reference density: the account
/// heading, the password field, and the line beneath it.
pub const PANEL_HEIGHT: u32 = 180;

/// The heading shown when the session could not name the signed-in account.
///
/// The lock still locks: whose password is wanted is a nicety, and failing to
/// resolve it must never be a reason to leave the screen open.
pub const UNNAMED_ACCOUNT: &str = "Locked";

/// What the field says while it is waiting for a password.
const HINT: &str = "Type your password and press Enter";

/// What the field says after the broker refused the password.
const REFUSED: &str = "That password was not accepted";

/// What the field says when no answer could be obtained at all.
///
/// Deliberately distinct from a refusal: "wrong password" and "I could not
/// ask" call for different reactions from the person at the keyboard.
const UNREACHABLE: &str = "Your password could not be checked just now";

/// What one input event did to the lock.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum LockOutcome {
    /// Nothing for the embedder to act on. The screen is still locked.
    Pending,
    /// The user proved their identity. The lock is already down — its window
    /// closed and the password erased — and the desktop is reachable again.
    Unlocked,
}

/// How the lock asks whether a password belongs to the signed-in user.
///
/// The lock never answers that itself. On a running system the desktop
/// program implements this over the per-console elevation broker; a test
/// injects an answer directly. Holding it behind a seam is what lets the
/// whole lock — its editing, its wording, its erasure of the password — be
/// exercised on the host without a kernel.
pub trait Unlocker {
    /// Whether `password` authenticates the user this session belongs to.
    ///
    /// # Errors
    ///
    /// Returns the [`Errno`] naming a failure to *reach* an answer — no
    /// broker, a transport fault, a reply that is not this protocol. That is
    /// distinct from `Ok(false)`, which is a real refusal of a real password.
    fn verify(&mut self, password: &str) -> Result<bool, Errno>;
}

/// One engaged lock: its full-screen window, the prompt panel headed with the
/// account name, and the masked field the password is typed into.
struct Engaged {
    wm: WindowId,
    panel: Panel,
    field: TextField,
}

/// The session's screen lock.
///
/// Idle until [`engage`](ScreenLock::engage) puts it up, then holding the
/// screen until the user is verified or the session is torn down.
#[derive(Default)]
pub struct ScreenLock {
    engaged: Option<Engaged>,
}

impl ScreenLock {
    /// An idle lock.
    #[must_use]
    pub const fn new() -> Self {
        Self { engaged: None }
    }

    /// Whether the screen is locked right now.
    ///
    /// While this is `true` the embedder MUST drain every pointer and
    /// keyboard event into [`handle`](Self::handle) and route none of them to
    /// the window manager, the taskbar, or any served window. That routing is
    /// what makes this a lock; the surface only hides the session.
    #[must_use]
    pub fn is_locked(&self) -> bool {
        self.engaged.is_some()
    }

    /// Lock the screen, naming `account` so the user knows whose password is
    /// wanted. An empty name heads the prompt with [`UNNAMED_ACCOUNT`].
    ///
    /// Returns whether the screen is now locked. A lock already up answers
    /// `true` and changes nothing. A surface the compositor cannot give
    /// answers `false` and locks nothing: a lock that cannot cover the screen
    /// is worse than no lock, because the user would walk away believing they
    /// were protected.
    pub fn engage(
        &mut self,
        account: &str,
        shell: &DesktopShell,
        compositor: &mut Compositor,
    ) -> bool {
        if self.engaged.is_some() {
            return true;
        }
        let screen = compositor.screen_rect();
        let account = if account.is_empty() {
            UNNAMED_ACCOUNT
        } else {
            account
        };
        let panel = Panel::new(account);
        let field = password_field();
        let Some(surface) = render_surface(&panel, &field, screen, shell) else {
            return false;
        };
        let wm = compositor.add_window(screen.origin, surface);
        compositor.raise(wm);
        self.engaged = Some(Engaged { wm, panel, field });
        true
    }

    /// Raise the lock above everything, so a window opened or raised behind
    /// it cannot surface over it.
    ///
    /// The embedder calls this immediately before each composite while
    /// locked. It is cheap and idempotent, and does nothing at all when the
    /// screen is not locked.
    pub fn keep_topmost(&self, compositor: &mut Compositor) {
        if let Some(engaged) = self.engaged.as_ref() {
            let _ = compositor.raise(engaged.wm);
        }
    }

    /// Apply one input event — pointer or key — to the lock.
    ///
    /// Keys edit the password; `Enter` offers it. The pointer places the
    /// caret and selects within the field, and reaches nothing else on
    /// screen. There is no key and no click that dismisses the lock without
    /// a verified password.
    ///
    /// Returns [`LockOutcome::Unlocked`] once the user has been verified, by
    /// which time the lock is already down.
    pub fn handle(
        &mut self,
        event: &InputEvent,
        unlocker: &mut dyn Unlocker,
        shell: &DesktopShell,
        compositor: &mut Compositor,
    ) -> LockOutcome {
        if let InputEvent::PointerMoved { to } = event {
            compositor.move_cursor(*to);
        }
        let screen = compositor.screen_rect();
        let theme = shell.session().active_theme();
        let font = prompt_font(theme);
        let Some(engaged) = self.engaged.as_mut() else {
            return LockOutcome::Pending;
        };
        let before = engaged.field.state();
        let (submitted, redraw) = match event {
            InputEvent::KeyPressed { key, modifiers } => {
                // Typing answers the question again, so a verdict on the
                // previous password stops standing over the new one.
                engaged.clear_verdict();
                let action = engaged.field.on_key(*key, *modifiers);
                // A key can move the caret without reporting an edit, and
                // arrives at human typing rate either way, so every one is
                // worth a frame.
                (matches!(action, Some(TextAction::Submitted)), true)
            }
            InputEvent::PointerMoved { .. }
            | InputEvent::PointerPressed { .. }
            | InputEvent::PointerReleased { .. } => {
                let bounds = field_rect(&engaged.panel, panel_rect(screen), theme);
                let local = to_window_space(event, screen);
                let action = engaged
                    .field
                    .on_pointer(&local, bounds, Scale::ONE, theme, font);
                let after = engaged.field.state();
                // Pointer motion is the one event that streams. Repainting
                // the whole screen for each one would rebuild a
                // screen-sized surface per sample for no visible
                // difference, so a frame is drawn only when the field's
                // rendering can actually have changed: it edited, its state
                // moved (a hover arriving or leaving), or a button is down
                // and the motion is extending a selection.
                let dragging = after.pointer == PointerState::Pressed;
                (
                    matches!(action, Some(TextAction::Submitted)),
                    action.is_some() || after != before || dragging,
                )
            }
            _ => (false, false),
        };
        if submitted && self.offer(unlocker) {
            self.release(compositor);
            return LockOutcome::Unlocked;
        }
        if redraw {
            self.repaint(shell, compositor);
        }
        LockOutcome::Pending
    }

    /// Take the lock down without verifying anybody, erasing the password.
    ///
    /// Used when the session is tearing the desktop down. It unlocks nothing:
    /// the session is ending, and the login prompt that replaces it asks for
    /// a password of its own.
    pub fn abandon(&mut self, compositor: &mut Compositor) {
        if let Some(mut engaged) = self.engaged.take() {
            engaged.field.set_text("");
            let _ = compositor.remove(engaged.wm);
        }
    }

    /// Repaint the lock, so a theme switch or a resolution change behind it
    /// redraws it at the right size in the appearance now in use.
    ///
    /// A surface that cannot be built leaves the previous frame up rather
    /// than uncovering the session.
    pub fn repaint(&self, shell: &DesktopShell, compositor: &mut Compositor) {
        let screen = compositor.screen_rect();
        let Some(engaged) = self.engaged.as_ref() else {
            return;
        };
        if let Some(surface) = render_surface(&engaged.panel, &engaged.field, screen, shell) {
            let _ = compositor.set_surface(engaged.wm, surface);
        }
        let _ = compositor.raise(engaged.wm);
    }

    /// Offer the typed password to `unlocker` and record what came back.
    ///
    /// Returns whether the user was verified. The password is erased before
    /// this returns, on every path — verified, refused, or unanswerable — so
    /// no later branch can be the one that forgot.
    fn offer(&mut self, unlocker: &mut dyn Unlocker) -> bool {
        let Some(engaged) = self.engaged.as_mut() else {
            return false;
        };
        let verdict = unlocker.verify(engaged.field.text());
        engaged.field.set_text("");
        match verdict {
            Ok(true) => true,
            Ok(false) => {
                engaged.show_verdict(REFUSED);
                false
            }
            Err(_) => {
                engaged.show_verdict(UNREACHABLE);
                false
            }
        }
    }

    /// Take the lock down after a successful verification.
    fn release(&mut self, compositor: &mut Compositor) {
        if let Some(engaged) = self.engaged.take() {
            let _ = compositor.remove(engaged.wm);
        }
    }
}

impl Engaged {
    /// Show `verdict` under the field and mark the field as refused, so the
    /// answer is both readable and visible in the field's own rendering
    /// rather than only in prose.
    fn show_verdict(&mut self, verdict: &str) {
        self.field.set_message(Some(verdict.to_string()));
        self.field
            .set_state(self.field.state().with_validation(ValidationState::Invalid));
    }

    /// Put the field back to its resting hint.
    fn clear_verdict(&mut self) {
        self.field.set_message(Some(HINT.to_string()));
        self.field
            .set_state(self.field.state().with_validation(ValidationState::Valid));
    }
}

/// The masked password field: bounded, pre-reserved, focused from the moment
/// the lock comes up so the user can simply start typing.
fn password_field() -> TextField {
    let mut field = TextField::new()
        .secret(MAX_PASSWORD)
        .with_placeholder("Password")
        .with_message(HINT);
    field.set_focused(true);
    field
}

/// Where the prompt panel sits on `screen`: centred horizontally, a third of
/// the way down, so it reads as a prompt rather than as a dialog stranded in
/// the middle of a blank screen.
///
/// A screen smaller than the panel gets the whole screen: the prompt is still
/// usable, and a lock that refused to draw on a small display would be a lock
/// that did not lock.
fn panel_rect(screen: Rect) -> Rect {
    let w = PANEL_WIDTH.min(screen.width);
    let h = PANEL_HEIGHT.min(screen.height);
    let x = i32::try_from((screen.width - w) / 2).unwrap_or(0);
    let y = i32::try_from((screen.height - h) / 3).unwrap_or(0);
    Rect::new(x, y, w, h)
}

/// Where the password field sits inside the prompt panel's `bounds`, in the
/// lock window's own pixel space.
///
/// The one definition of the field's placement, so the paint and the pointer
/// hit test resolve the same rectangle rather than each deriving its own. A
/// panel too small to have a content area yields the whole panel: the field
/// is then cramped but still there, which beats a prompt with nothing to
/// type into.
fn field_rect(panel: &Panel, bounds: Rect, theme: &Theme) -> Rect {
    panel
        .content_rect(bounds, Scale::ONE, theme)
        .unwrap_or(bounds)
}

/// Rebase a pointer event from screen space into the lock window's space.
///
/// The lock window covers the screen from its top-left, so the two spaces
/// differ only by the screen rectangle's own origin — which is zero on every
/// single-output desktop, and correct rather than assumed on any other.
fn to_window_space(event: &InputEvent, screen: Rect) -> InputEvent {
    match event {
        InputEvent::PointerMoved { to } => InputEvent::PointerMoved {
            to: Point::new(to.x - screen.origin.x, to.y - screen.origin.y),
        },
        other => *other,
    }
}

/// Paint the whole screen: an opaque cover, then the prompt panel and the
/// masked field on it.
fn render_surface(
    panel: &Panel,
    field: &TextField,
    screen: Rect,
    shell: &DesktopShell,
) -> Option<Surface> {
    let theme = shell.session().active_theme();
    let font = prompt_font(theme);
    let mut surface = Surface::new(screen.width, screen.height)?;
    surface.fill(Color::from(theme.palette().desktop));

    let bounds = panel_rect(screen);
    panel.render(&mut surface, bounds, Scale::ONE, theme, font);
    field.render(
        &mut surface,
        field_rect(panel, bounds, theme),
        Scale::ONE,
        theme,
        font,
    );
    Some(surface)
}

/// The prompt's text font: the theme's ordinary interface-text role at the
/// density the panel's unscaled extents are authored in, so the paint and the
/// hit test agree on one font.
fn prompt_font(theme: &Theme) -> BitmapFont {
    BitmapFont::for_role(theme.fonts(), TextRole::Body, Scale::ONE)
}
