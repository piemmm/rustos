//! [`CredentialSheet`]: the one surface that asks for an account and its
//! password so a more-privileged program can be started as that account.
//!
//! Two places on the desktop ask that question — the session, when a command
//! it may not perform is chosen, and the Settings application, when a machine
//! setting is applied by re-running the tool that owns the store — and they
//! may not depend on one another. Two credential surfaces is one too many:
//! the wording, the focus order, the "an empty field is never offered" rule,
//! and the secret's hygiene would each have two places to get wrong.
//!
//! The sheet owns none of that context. It knows nothing of a compositor, a
//! window, an account database, or IPC: an owner gives it events and a
//! rectangle, takes back a painted surface and a [`CredentialAction`], and
//! performs the exchange itself. Whether it is drawn in a window of its own
//! or over a client's content is the owner's business.
//!
//! # The secret
//!
//! The password is held only in the masked field's bounded, pre-reserved
//! buffer, which cannot reallocate while filling and zeroises every byte it
//! discards — including on drop. Dropping the sheet therefore leaves no
//! plaintext behind, whichever way the question ended.

use alloc::string::String;
use alloc::vec;

use tairix_geometry::{to_i32, Point, Rect, Region, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_raster::Surface;
use tairix_theme::Theme;

use crate::button::{Button, ButtonContent};
use crate::damage;
use crate::decision::{Dialog, DialogAction};
use crate::state::{ControlRole, FocusState};
use crate::text::TextField;

/// The sheet's width in logical pixels: wide enough for the explaining
/// sentence on one line at the reference density, and no wider — it is a
/// question, not a document.
pub const CREDENTIAL_WIDTH: u32 = 460;

/// The sheet's height in logical pixels: the title, the sentence, the two
/// fields, and the action band beneath them.
pub const CREDENTIAL_HEIGHT: u32 = 250;

/// Left and right inset of the fields within the sheet, in logical pixels.
const FIELD_INSET: u32 = 18;

/// One field's height in logical pixels.
const FIELD_HEIGHT: u32 = 32;

/// Top of the account field within the sheet, in logical pixels: below the
/// dialog's title and message.
const FIELD_TOP: u32 = 96;

/// Vertical distance between the two fields' tops, in logical pixels.
const FIELD_PITCH: u32 = 44;

/// Longest account name the sheet accepts. A login name is short; a longer
/// one could only be a paste of something else.
const MAX_ACCOUNT: usize = 64;

/// Longest password the sheet accepts, which is also the masked field's
/// reserved capacity.
const MAX_SECRET: usize = 128;

/// Index of the cancelling button in the action band. Leading, and focus
/// starts in the account field rather than on a button, so no stray
/// keystroke offers a half-typed credential.
const CANCEL_ACTION: usize = 0;

/// Index of the continuing button.
const CONTINUE_ACTION: usize = 1;

/// The account field's placeholder, naming what is wanted.
const ACCOUNT_LABEL: &str = "Account name";

/// The secret field's placeholder.
const SECRET_LABEL: &str = "Password";

/// What the surface states when the authority refused the attempt.
///
/// A wrong password, an unknown account, and a locked account are one
/// indistinguishable refusal, so this says exactly that much and never
/// guesses which it was.
pub const CREDENTIAL_REFUSED_REASON: &str = "That account and password were not accepted.";

/// What the surface states when the account authenticated but the program
/// did not run, so the user is not told to check a password that was
/// accepted.
pub const CREDENTIAL_NOT_STARTED_REASON: &str =
    "The account was accepted, but the application did not start.";

/// What one input event concluded for a [`CredentialSheet`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CredentialAction {
    /// The user cancelled. Nothing was offered; the owner takes the sheet
    /// down, and dropping it erases the secret.
    Cancelled,
    /// Both fields are filled and the user asked to proceed. The owner reads
    /// [`account`](CredentialSheet::account) and
    /// [`secret`](CredentialSheet::secret) and performs the exchange.
    Offered,
}

/// Which part of the sheet holds the keyboard.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Focus {
    Account,
    Secret,
    Cancel,
    Continue,
}

impl Focus {
    /// The next focus in tab order, wrapping.
    const fn next(self) -> Self {
        match self {
            Self::Account => Self::Secret,
            Self::Secret => Self::Cancel,
            Self::Cancel => Self::Continue,
            Self::Continue => Self::Account,
        }
    }
}

/// A question asking for an account that may perform some command, and that
/// account's password.
///
/// The sheet performs no privileged work and holds no authority: it collects
/// what was typed and reports [`CredentialAction`]; the owner posts it to
/// whatever authenticates.
pub struct CredentialSheet {
    dialog: Dialog,
    account: TextField,
    secret: TextField,
    focus: Focus,
    /// Where the pointer last was, in the sheet's own space, so a press
    /// resolves against the field rectangles it was actually over.
    pointer: Point,
}

impl CredentialSheet {
    /// A sheet titled `title`, explaining what the account is wanted for
    /// with `purpose`, with the keyboard in the account field.
    #[must_use]
    pub fn new(title: &str, purpose: &str) -> Self {
        let mut account = TextField::new()
            .with_max_len(MAX_ACCOUNT)
            .with_message(ACCOUNT_LABEL);
        account.set_focused(true);
        Self {
            dialog: build_dialog(title, purpose, None),
            account,
            secret: TextField::new()
                .secret(MAX_SECRET)
                .with_message(SECRET_LABEL),
            focus: Focus::Account,
            pointer: Point::ORIGIN,
        }
    }

    /// The account name as typed.
    #[must_use]
    pub fn account(&self) -> &str {
        self.account.text()
    }

    /// The password as typed.
    ///
    /// A secret: a caller reads it to perform one exchange and lets it go;
    /// it is never stored, logged, or copied into a buffer that outlives the
    /// call.
    #[must_use]
    pub fn secret(&self) -> &str {
        self.secret.text()
    }

    /// The refusal the sheet is currently stating, if any.
    #[must_use]
    pub fn stated_reason(&self) -> Option<&str> {
        self.dialog.reason()
    }

    /// State `reason` and clear the password for another attempt.
    ///
    /// The account name is left as typed: it is not the secret, and retyping
    /// it would only make a correct name harder to keep. Discarding the
    /// password's buffer zeroises it.
    pub fn refuse(&mut self, reason: &str) {
        self.dialog = build_dialog(
            self.dialog.title(),
            self.dialog.message().unwrap_or_default(),
            Some(reason),
        );
        self.secret.set_text("");
        self.set_focus(Focus::Secret);
    }

    /// The sheet's rectangle centred in `within`, at its own logical size.
    ///
    /// An owner drawing the sheet over its own content places it here; one
    /// that gives the sheet a window of its own uses the window's whole
    /// extent instead.
    #[must_use]
    pub fn centred_in(within: Rect, scale: Scale) -> Rect {
        let width = scale.scale_length(CREDENTIAL_WIDTH).min(within.width);
        let height = scale.scale_length(CREDENTIAL_HEIGHT).min(within.height);
        let left = within
            .left()
            .saturating_add(to_i32(within.width.saturating_sub(width) / 2));
        let top = within
            .top()
            .saturating_add(to_i32(within.height.saturating_sub(height) / 2));
        Rect::new(left, top, width, height)
    }

    /// The physical rectangle of field `index` (`0` account, `1` password)
    /// within a sheet drawn at `bounds`.
    ///
    /// The one definition of the fields' geometry, so the paint, the hit
    /// test, and the pointer routing all resolve the same rectangles rather
    /// than each re-deriving them. Public so a host-side observer clicks
    /// where the field actually is rather than restating the layout.
    #[must_use]
    pub fn field_rect(bounds: Rect, scale: Scale, index: u32) -> Rect {
        let inset = scale.scale_length(FIELD_INSET);
        let top = scale.scale_length(FIELD_TOP + FIELD_PITCH * index);
        let width = bounds.width.saturating_sub(inset.saturating_mul(2));
        Rect::new(
            bounds.left().saturating_add(to_i32(inset)),
            bounds.top().saturating_add(to_i32(top)),
            width,
            scale.scale_length(FIELD_HEIGHT),
        )
    }

    /// Paint the sheet at `bounds`: the dialog chrome, then the two fields
    /// over its body.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        self.dialog.render(surface, bounds, scale, theme);
        self.account
            .render(surface, Self::field_rect(bounds, scale, 0), scale, theme);
        self.secret
            .render(surface, Self::field_rect(bounds, scale, 1), scale, theme);
    }

    /// Apply one input event — pointer or key — to a sheet drawn at
    /// `bounds`.
    ///
    /// `Escape` cancels outright. `Tab` moves the keyboard on. `Enter`
    /// offers the credentials from either field, so a password can be
    /// submitted without reaching for the button. Everything else edits
    /// whichever field holds the keyboard.
    pub fn handle(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<CredentialAction> {
        match event {
            InputEvent::KeyPressed { key, modifiers } => {
                self.key(*key, *modifiers, bounds, scale, damage)
            }
            InputEvent::PointerMoved { .. }
            | InputEvent::PointerPressed { .. }
            | InputEvent::PointerReleased { .. } => {
                self.pointer(event, bounds, scale, theme, damage)
            }
            _ => None,
        }
    }

    /// Apply one key press.
    fn key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        bounds: Rect,
        scale: Scale,
        damage: &mut Region,
    ) -> Option<CredentialAction> {
        match key {
            Key::Named(NamedKey::Escape) => Some(CredentialAction::Cancelled),
            Key::Named(NamedKey::Tab) => {
                self.set_focus(self.focus.next());
                damage.add(bounds);
                None
            }
            Key::Named(NamedKey::Enter) => match self.focus {
                // A button holds the keyboard: it decides, not the fields.
                Focus::Cancel => Some(CredentialAction::Cancelled),
                Focus::Account | Focus::Secret | Focus::Continue => self.offer(bounds, damage),
            },
            other => {
                let field = match self.focus {
                    Focus::Account => Some((&mut self.account, 0)),
                    Focus::Secret => Some((&mut self.secret, 1)),
                    // A focused button takes no text.
                    Focus::Cancel | Focus::Continue => None,
                };
                if let Some((field, index)) = field {
                    let rect = Self::field_rect(bounds, scale, index);
                    let _ = field.on_key(other, modifiers, rect, damage);
                }
                damage.add(bounds);
                None
            }
        }
    }

    /// Apply one pointer event: a press in a field moves the keyboard there
    /// and places the caret; a completed click on a button decides.
    fn pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<CredentialAction> {
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }
        if let InputEvent::PointerPressed {
            button: PointerButton::Primary,
        } = event
        {
            if let Some(focus) = self.field_under_pointer(bounds, scale) {
                self.set_focus(focus);
            }
        }
        let mut own = damage::sink();
        let _ = self.account.on_pointer(
            event,
            Self::field_rect(bounds, scale, 0),
            scale,
            theme,
            &mut own,
        );
        let _ = self.secret.on_pointer(
            event,
            Self::field_rect(bounds, scale, 1),
            scale,
            theme,
            &mut own,
        );
        let action = self
            .dialog
            .on_pointer(event, bounds, scale, theme, &mut own);
        for rect in own.rects() {
            damage.add(*rect);
        }
        match action {
            Some(DialogAction::ActionActivated {
                index: CONTINUE_ACTION,
            }) => self.offer(bounds, damage),
            Some(DialogAction::ActionActivated {
                index: CANCEL_ACTION,
            }) => Some(CredentialAction::Cancelled),
            // A band that reported some other index is not one of the two
            // buttons this sheet built; deciding nothing leaves it up rather
            // than guessing what the user meant.
            _ => None,
        }
    }

    /// Offer what has been typed, or move the keyboard to the field that is
    /// still empty.
    ///
    /// An empty field is never offered: there is nothing to check, and
    /// asking would spend an audited attempt against the account.
    fn offer(&mut self, bounds: Rect, damage: &mut Region) -> Option<CredentialAction> {
        if self.account.text().is_empty() || self.secret.text().is_empty() {
            let empty = if self.account.text().is_empty() {
                Focus::Account
            } else {
                Focus::Secret
            };
            self.set_focus(empty);
            damage.add(bounds);
            return None;
        }
        Some(CredentialAction::Offered)
    }

    /// Which field the pointer is over, if either.
    fn field_under_pointer(&self, bounds: Rect, scale: Scale) -> Option<Focus> {
        if Self::field_rect(bounds, scale, 0).contains(self.pointer) {
            return Some(Focus::Account);
        }
        Self::field_rect(bounds, scale, 1)
            .contains(self.pointer)
            .then_some(Focus::Secret)
    }

    /// Give the keyboard to `focus` and take it from everything else, so
    /// exactly one focus ring is drawn and `Enter` can only mean one thing.
    fn set_focus(&mut self, focus: Focus) {
        self.focus = focus;
        self.account.set_focused(focus == Focus::Account);
        self.secret.set_focused(focus == Focus::Secret);
        for (index, button) in self.dialog.actions_mut().iter_mut().enumerate() {
            let focused = match focus {
                Focus::Cancel => index == CANCEL_ACTION,
                Focus::Continue => index == CONTINUE_ACTION,
                Focus::Account | Focus::Secret => false,
            };
            let mut state = button.state();
            state.focus = if focused {
                FocusState::FOCUSED
            } else {
                FocusState::default()
            };
            button.set_state(state);
        }
    }
}

/// Build the sheet's dialog: the cancelling button leading, the continuing
/// one trailing, and neither focused — the keyboard starts in the account
/// field.
fn build_dialog(title: &str, purpose: &str, reason: Option<&str>) -> Dialog {
    let dialog = Dialog::new(String::from(title))
        .with_message(String::from(purpose))
        .with_actions(vec![
            Button::labelled("Cancel"),
            Button::new(
                ButtonContent::Label(String::from("Continue")),
                ControlRole::Neutral,
            ),
        ]);
    match reason {
        Some(reason) => dialog.with_reason(String::from(reason)),
        None => dialog,
    }
}

#[cfg(test)]
#[path = "credential_tests.rs"]
mod credential_tests;
