//! The authentication surface: the panel, the masked field, the notice, and
//! the state machine that turns one input event into a verdict.

use alloc::string::ToString;

use tairix_controls::{Panel, PointerState, TextAction, TextField, ValidationState};
use tairix_geometry::{Rect, Scale};
use tairix_input::InputEvent;
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

/// Longest secret the surface will hold, in characters.
///
/// The same bound the text login prompt reads a line at, and far below what
/// the elevation wire format accepts. It is a fail-closed memory bound, not
/// a policy on what a password may be: it exists so the field can reserve
/// its buffer once and never grow it, which is what keeps a copy of the
/// secret out of a freed heap block.
pub const MAX_PASSWORD: usize = 256;

/// The heading shown when the embedder could not name the account.
///
/// The surface still asks: whose secret is wanted is a nicety, and failing
/// to resolve it must never be a reason to let anybody through.
pub const UNNAMED_ACCOUNT: &str = "Locked";

/// The panel's width in pixels at the reference density.
pub(crate) const PANEL_WIDTH: u32 = 420;

/// The panel's height in pixels at the reference density: the account
/// heading, the secret field, and the line beneath it.
pub(crate) const PANEL_HEIGHT: u32 = 180;

/// What the field says while it is waiting for a secret.
pub(crate) const HINT: &str = "Type your password and press Enter";

/// What the field says after the verifier refused the secret.
pub(crate) const REFUSED: &str = "That password was not accepted";

/// What the field says when no answer could be obtained at all.
///
/// Deliberately distinct from a refusal: "wrong password" and "I could not
/// ask" call for different reactions from the person at the keyboard.
pub(crate) const UNREACHABLE: &str = "Your password could not be checked just now";

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

/// How the surface asks whether a secret belongs to the account.
///
/// The surface never answers that itself: it holds no credential store, no
/// key, and no authority. On a running system the embedder implements this
/// over whatever authenticates for it — the per-console elevation broker
/// behind the desktop's screen lock, the session authority behind the login
/// greeter — and a test injects an answer directly. Holding it behind a seam
/// is what lets the whole surface — its editing, its wording, its erasure of
/// the secret — be exercised on the host without a kernel.
pub trait Verifier {
    /// Whether `secret` authenticates the account this surface is asking
    /// for.
    fn verify(&mut self, secret: &str) -> Verdict;
}

/// What the surface paints behind its panel.
///
/// The caller chooses it, so an embedder that already holds a decoded
/// wallpaper can hand one in without this engine ever learning to decode an
/// image: decoding is the caller's sandboxed business, painting is this
/// crate's.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Backdrop {
    /// The active theme's flat desktop colour, fully opaque.
    Desktop,
}

/// What one input event did to the surface.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Outcome {
    redraw: bool,
    verified: bool,
}

impl Outcome {
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
}

/// One authentication surface: the panel headed with the account name, the
/// masked field the secret is typed into, and the notice under it.
///
/// The surface is modal and total by construction. It offers no cancel, no
/// timeout, and no affordance that concludes it: [`on_event`](Self::on_event)
/// reports [`Outcome::verified`] only when a [`Verifier`] answered
/// [`Verdict::Verified`], so no key and no click can conclude it without one.
/// Enforcing that *nothing else* on the machine sees those events is the
/// embedder's half of the contract; a surface cannot do it from the inside.
///
/// Nothing here rate-limits or counts attempts, deliberately. The authority
/// behind the [`Verifier`] owns that policy and audits every attempt against
/// the account; a second policy on this side would be a second place to get
/// it wrong, and one that could not slow down an attacker holding the
/// keyboard anyway.
pub struct AuthSurface {
    panel: Panel,
    field: TextField,
    notice: &'static str,
}

impl AuthSurface {
    /// A surface asking for `account`'s secret, focused and ready to type
    /// into. An empty name heads it with [`UNNAMED_ACCOUNT`].
    #[must_use]
    pub fn new(account: &str) -> Self {
        let account = if account.is_empty() {
            UNNAMED_ACCOUNT
        } else {
            account
        };
        Self {
            panel: Panel::new(account),
            field: secret_field(),
            notice: HINT,
        }
    }

    /// The line currently shown under the field: the resting hint, or the
    /// answer to the last secret offered.
    #[must_use]
    pub const fn notice(&self) -> &str {
        self.notice
    }

    /// Apply one input event — pointer or key, in the surface's own
    /// coordinate space.
    ///
    /// Keys edit the secret; `Enter` offers it to `ctx.verifier`. The
    /// pointer places the caret and selects within the field, and reaches
    /// nothing else. The secret is erased before this returns on every path
    /// out of an offer — accepted, refused, or unanswerable — so no later
    /// branch can be the one that forgot.
    pub fn on_event(&mut self, event: &InputEvent, ctx: &mut EventContext<'_>) -> Outcome {
        let before = self.field.state();
        let (submitted, redraw) = match event {
            InputEvent::KeyPressed { key, modifiers } => {
                // Typing asks the question again, so a verdict on the
                // previous secret stops standing over the new one.
                self.show(HINT, ValidationState::Valid);
                let action = self.field.on_key(*key, *modifiers);
                // A key can move the caret without reporting an edit, and
                // arrives at human typing rate either way, so every one is
                // worth a frame.
                (matches!(action, Some(TextAction::Submitted)), true)
            }
            InputEvent::PointerMoved { .. }
            | InputEvent::PointerPressed { .. }
            | InputEvent::PointerReleased { .. } => {
                let bounds = self.field_rect(ctx.screen, ctx.scale, ctx.theme);
                let action = self.field.on_pointer(event, bounds, ctx.scale, ctx.theme);
                let after = self.field.state();
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
        Outcome {
            redraw,
            verified: submitted && self.offer(&mut *ctx.verifier),
        }
    }

    /// Paint the whole frame: the backdrop, then the panel and the masked
    /// field on it.
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
        backdrop: Backdrop,
    ) -> Option<Surface> {
        if screen.width == 0 || screen.height == 0 {
            return None;
        }
        let mut surface = Surface::new(screen.width, screen.height)?;
        match backdrop {
            Backdrop::Desktop => surface.fill(Color::from(theme.palette().desktop)),
        }

        self.panel
            .render(&mut surface, panel_rect(screen, scale), scale, theme);
        self.field.render(
            &mut surface,
            self.field_rect(screen, scale, theme),
            scale,
            theme,
        );
        Some(surface)
    }

    /// Where the secret field sits on `screen`.
    ///
    /// The one definition of the field's placement, so the paint and the
    /// pointer hit test resolve the same rectangle rather than each deriving
    /// its own. A panel too small to have a content area yields the whole
    /// panel: the field is then cramped but still there, which beats a
    /// prompt with nothing to type into.
    #[must_use]
    pub fn field_rect(&self, screen: Rect, scale: Scale, theme: &Theme) -> Rect {
        let bounds = panel_rect(screen, scale);
        self.panel
            .content_rect(bounds, scale, theme)
            .unwrap_or(bounds)
    }

    /// Offer the typed secret to `verifier` and record what came back.
    ///
    /// Returns whether the account was verified. The secret is erased before
    /// this returns, on every path.
    fn offer(&mut self, verifier: &mut dyn Verifier) -> bool {
        let verdict = verifier.verify(self.field.text());
        self.field.set_text("");
        match verdict {
            Verdict::Verified => true,
            Verdict::Refused => {
                self.show(REFUSED, ValidationState::Invalid);
                false
            }
            Verdict::Unreachable => {
                self.show(UNREACHABLE, ValidationState::Invalid);
                false
            }
        }
    }

    /// Show `notice` under the field at `validation`, so the answer is both
    /// readable and visible in the field's own rendering rather than only in
    /// prose.
    fn show(&mut self, notice: &'static str, validation: ValidationState) {
        self.notice = notice;
        self.field.set_message(Some(notice.to_string()));
        self.field
            .set_state(self.field.state().with_validation(validation));
    }
}

/// Where the panel sits on `screen`: centred horizontally, a third of the
/// way down, so it reads as a prompt rather than as a dialog stranded in the
/// middle of a blank screen.
///
/// A screen smaller than the panel gets the whole screen: the prompt is
/// still usable, and a surface that refused to draw on a small display would
/// be one that did not ask.
#[must_use]
pub fn panel_rect(screen: Rect, scale: Scale) -> Rect {
    let w = scale.scale_length(PANEL_WIDTH).min(screen.width);
    let h = scale.scale_length(PANEL_HEIGHT).min(screen.height);
    let x = screen
        .origin
        .x
        .saturating_add(i32::try_from((screen.width - w) / 2).unwrap_or(0));
    let y = screen
        .origin
        .y
        .saturating_add(i32::try_from((screen.height - h) / 3).unwrap_or(0));
    Rect::new(x, y, w, h)
}

/// The masked secret field: bounded, pre-reserved, focused from the moment
/// the surface comes up so the user can simply start typing.
fn secret_field() -> TextField {
    let mut field = TextField::new()
        .secret(MAX_PASSWORD)
        .with_placeholder("Password")
        .with_message(HINT);
    field.set_focused(true);
    field
}
