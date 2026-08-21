//! The desktop session's **credential prompt** for a command the signed-in
//! user is not authorised to perform.
//!
//! Setting the machine's clock needs `CAP_TIME_SET`, which a desktop session
//! does not hold and must never be able to acquire. So the session does not
//! perform the command: it asks for an account that *may*, and hands the
//! offered credentials to the per-console elevation broker — the login
//! supervisor that started this session — which re-authenticates the account
//! itself, audits the decision, and starts the program as that account. The
//! session learns only whether it was refused.
//!
//! The prompt is the session's own window, drawn with the shared
//! `lib/controls` dialog and two shared text fields, and it follows the
//! confirmation prompt's shape: one slot (a second request while one is
//! showing is refused rather than stacking a second prompt), and a typed
//! conclusion the embedder acts on once the window is already closed.
//!
//! # The secret
//!
//! The password is typed into a masked [`TextField::secret`] field, which
//! bounds its buffer so it can never reallocate while filling and zeroises
//! every byte it discards — including on drop. The prompt therefore leaves no
//! plaintext behind on any exit: a cancellation, a refusal that clears the
//! field for another try, a successful launch, or the session being torn down
//! around it. Neither the offered password nor the account name reaches the
//! system log; only the broker audits the attempt.
//!
//! # Refusals
//!
//! A refused attempt leaves the prompt up with the reason stated and the
//! password cleared, so the user can try again without the surface pretending
//! anything happened. The broker refuses a wrong password, an unknown
//! account, and a locked account indistinguishably, and this prompt repeats
//! exactly what it was told rather than guessing which it was.

use alloc::string::String;

use tairix_abi::input::KeyInput;
use tairix_abi::Errno;
use tairix_controls::{
    damage, Button, ButtonContent, ControlRole, Dialog, DialogAction, FocusState, TextField,
};
use tairix_geometry::Scale;
use tairix_wm::{
    Compositor, InputEvent, Key, Modifiers, NamedKey, Point, PointerButton, Rect, Surface, WindowId,
};

use crate::shell::DesktopShell;

/// The prompt window's width in logical pixels.
///
/// Wide enough for the explaining sentence on one line at the reference
/// density, and no wider: it is a question, not a document.
pub const WIN_WIDTH: u32 = 460;

/// The prompt window's height in logical pixels: the title, the sentence, the
/// two fields, and the action band beneath them.
pub const WIN_HEIGHT: u32 = 250;

/// Top-left of the prompt window, in screen pixels.
///
/// One deterministic spot, clear of the first window-cascade slots and of the
/// confirmation prompt's, exported so a host-side observer drives the prompt
/// where the session actually puts it rather than at a re-derived guess.
pub const ELEVATE_ORIGIN: Point = Point::new(280, 160);

/// Left and right inset of the fields within the window, in logical pixels.
const FIELD_INSET: u32 = 18;

/// One field's height in logical pixels.
const FIELD_HEIGHT: u32 = 32;

/// Top of the account-name field within the window, in logical pixels: below
/// the dialog's title and message.
const FIELD_TOP: u32 = 96;

/// Vertical distance between the two fields' tops, in logical pixels.
const FIELD_PITCH: u32 = 44;

/// Longest account name the prompt accepts. A login name is short; a longer
/// one could only be a paste of something else.
const MAX_ACCOUNT: usize = 64;

/// Longest password the prompt accepts, which is also the masked field's
/// reserved capacity.
const MAX_SECRET: usize = 128;

/// Index of the cancelling button in the dialog's action band. Leading, and
/// focus starts in the account field rather than on a button, so no stray
/// keystroke offers a half-typed credential.
const CANCEL_ACTION: usize = 0;

/// Index of the continuing button.
const CONTINUE_ACTION: usize = 1;

/// What the prompt states when the broker refused the attempt.
///
/// The broker refuses a wrong password, an unknown account, and a locked
/// account with one indistinguishable answer, so this says exactly that much
/// and never guesses which it was.
pub const REFUSED_REASON: &str = "That account and password were not accepted.";

/// What the prompt states when the account authenticated but the program did
/// not start, so the user is not told to check a password that was accepted.
pub const NOT_STARTED_REASON: &str = "The account was accepted, but the application did not start.";

/// The exchange the prompt performs once the user offers credentials.
///
/// Injected so the whole prompt — its editing, its wording, its refusal
/// handling, its erasure of the secret — is exercised on the host without a
/// kernel. The implementation posts to the console's elevation broker; the
/// prompt itself holds no authority and performs no privileged work.
pub trait Elevator {
    /// Offer `password` for `username` and, if it authenticates, start
    /// `program` as that account without waiting for it, answering its pid.
    ///
    /// # Errors
    ///
    /// [`Errno::PermissionDenied`] for a refused authentication — the broker
    /// gives one indistinguishable answer for a wrong password, an unknown
    /// account, and a locked one. Any other code reports a mechanical
    /// failure, such as a program that would not start.
    fn launch(&mut self, username: &str, password: &str, program: &str) -> Result<i32, Errno>;
}

/// How the prompt ended, or that it has not.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PromptOutcome {
    /// Still up. Either nothing conclusive happened, or an attempt was
    /// refused and the prompt is waiting for another.
    Pending,
    /// An account was accepted and the program started as it. The prompt is
    /// already down and its secret erased.
    Started {
        /// The started program's pid, as the broker reported it.
        pid: i32,
    },
    /// The user cancelled, dismissed, or the prompt was abandoned. Nothing
    /// was started and nothing was offered.
    Cancelled,
}

/// Which part of the prompt holds the keyboard.
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

/// One showing prompt: the program it will start, its compositor window, the
/// dialog chrome, and the two fields.
struct ActivePrompt {
    program: String,
    wm: WindowId,
    dialog: Dialog,
    account: TextField,
    secret: TextField,
    focus: Focus,
    /// Where the pointer last was, in the prompt window's own space, so a
    /// press resolves against the field rectangles it was actually over.
    pointer: Point,
}

/// The parts of a prompt that exist before it has a window.
///
/// The first frame has to be painted to know whether the prompt can be shown
/// at all, and painting needs the fields — so they are built here and the
/// window is added around them, rather than a window handle being invented
/// and then corrected.
struct PendingPrompt {
    program: String,
    dialog: Dialog,
    account: TextField,
    secret: TextField,
}

/// The session's credential-prompt slot.
///
/// Idle until a command the session may not perform is chosen, then holding
/// exactly one prompt until the user cancels it or an account is accepted.
#[derive(Default)]
pub struct ElevatePrompt {
    active: Option<ActivePrompt>,
}

impl ElevatePrompt {
    /// An idle prompt.
    #[must_use]
    pub const fn new() -> Self {
        Self { active: None }
    }

    /// The compositor window of the showing prompt, if one is up.
    ///
    /// The embedder routes this window's input into
    /// [`handle`](Self::handle) rather than to any served window.
    #[must_use]
    pub fn wm_id(&self) -> Option<WindowId> {
        self.active.as_ref().map(|active| active.wm)
    }

    /// The program the showing prompt would start, if one is up.
    #[must_use]
    pub fn pending(&self) -> Option<&str> {
        self.active.as_ref().map(|active| active.program.as_str())
    }

    /// Ask for an account that may run `program`, explaining the command with
    /// `purpose`.
    ///
    /// Returns whether the prompt came up. A prompt already showing, or a
    /// window the compositor could not give, answers `false`: nothing is
    /// offered and nothing is started, so a prompt that cannot be shown can
    /// never be taken for an answer.
    pub fn ask(
        &mut self,
        program: &str,
        purpose: &str,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
    ) -> bool {
        if self.active.is_some() {
            return false;
        }
        let mut account = TextField::new()
            .with_max_len(MAX_ACCOUNT)
            .with_message(ACCOUNT_LABEL);
        account.set_focused(true);
        let pending = PendingPrompt {
            program: String::from(program),
            dialog: build_dialog(purpose),
            account,
            secret: TextField::new()
                .secret(MAX_SECRET)
                .with_message(SECRET_LABEL),
        };
        let Some(surface) = render_surface(
            &pending.dialog,
            &pending.account,
            &pending.secret,
            compositor.scale(),
            shell,
        ) else {
            return false;
        };
        let Some(wm) = shell.open_window(compositor, ELEVATE_ORIGIN, surface, TITLE) else {
            return false;
        };
        self.active = Some(ActivePrompt {
            program: pending.program,
            wm,
            dialog: pending.dialog,
            account: pending.account,
            secret: pending.secret,
            focus: Focus::Account,
            pointer: Point::ORIGIN,
        });
        true
    }

    /// Apply one input event — pointer or key — to the showing prompt.
    ///
    /// `Escape` cancels outright. `Tab` moves the keyboard on. `Enter` offers
    /// the credentials from either field, so a password can be submitted
    /// without reaching for the button. Everything else edits whichever field
    /// holds the keyboard.
    ///
    /// Returns the outcome; on anything other than
    /// [`PromptOutcome::Pending`] the prompt window is already closed and its
    /// secret erased.
    pub fn handle(
        &mut self,
        event: &InputEvent,
        elevator: &mut dyn Elevator,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
    ) -> PromptOutcome {
        if self.active.is_none() {
            return PromptOutcome::Pending;
        }
        match event {
            InputEvent::KeyPressed { key, modifiers } => {
                self.key(*key, *modifiers, elevator, shell, compositor)
            }
            InputEvent::PointerMoved { .. }
            | InputEvent::PointerPressed { .. }
            | InputEvent::PointerReleased { .. } => {
                self.pointer(event, elevator, shell, compositor)
            }
            _ => PromptOutcome::Pending,
        }
    }

    /// Apply one wire key record to the showing prompt.
    ///
    /// The serve loop reaches this prompt by window id and carries the
    /// record the window server routes, so the record is decoded through the
    /// session's one wire-to-routing translation rather than a second one
    /// written here.
    pub fn handle_key(
        &mut self,
        record: &KeyInput,
        elevator: &mut dyn Elevator,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
    ) -> PromptOutcome {
        self.handle(
            &crate::keyboard::to_input_event(*record),
            elevator,
            shell,
            compositor,
        )
    }

    /// Apply one primary-button click at the prompt-window-local position
    /// `local`.
    ///
    /// A press-and-release at the same point, which is what the router
    /// reports: a field takes the keyboard and the caret, and a button
    /// decides.
    pub fn handle_click(
        &mut self,
        local: Point,
        elevator: &mut dyn Elevator,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
    ) -> PromptOutcome {
        for event in [
            InputEvent::PointerMoved { to: local },
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            },
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            },
        ] {
            let outcome = self.handle(&event, elevator, shell, compositor);
            if outcome != PromptOutcome::Pending {
                return outcome;
            }
        }
        PromptOutcome::Pending
    }

    /// Take the prompt down without offering anything.
    ///
    /// Used when the session is tearing the desktop down or the theme it was
    /// drawn against is gone. The secret goes with the field, which zeroises
    /// its buffer as it is dropped, so an abandoned prompt leaves no
    /// plaintext behind.
    pub fn abandon(&mut self, shell: &mut DesktopShell, compositor: &mut Compositor) {
        let _ = self.conclude(shell, compositor, None);
    }

    /// Repaint the showing prompt, so a theme switch behind it redraws it in
    /// the appearance now in use. A surface that cannot be built leaves the
    /// previous frame up rather than failing.
    pub fn repaint(&mut self, shell: &mut DesktopShell, compositor: &mut Compositor) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        if let Some(surface) = render_surface(
            &active.dialog,
            &active.account,
            &active.secret,
            compositor.scale(),
            shell,
        ) {
            let _ = compositor.set_surface(active.wm, surface);
        }
    }

    /// The refusal the showing prompt is stating, if it is stating one.
    ///
    /// Host-test observation only, so no shipped path can read a prompt's
    /// internals: the embedder never needs it, because the prompt words its
    /// own refusals into its own window.
    #[cfg(test)]
    pub(crate) fn stated_reason(&self) -> Option<&str> {
        self.active
            .as_ref()
            .and_then(|active| active.dialog.reason())
    }

    /// The account name typed into the showing prompt. Host-test observation
    /// only.
    #[cfg(test)]
    pub(crate) fn account_text(&self) -> Option<&str> {
        self.active.as_ref().map(|active| active.account.text())
    }

    /// How many characters the password field is holding. Host-test
    /// observation only.
    ///
    /// Deliberately a length and never the buffer, so proving the field was
    /// cleared never creates a path that hands the secret out.
    #[cfg(test)]
    pub(crate) fn secret_len(&self) -> usize {
        self.active
            .as_ref()
            .map_or(0, |active| active.secret.text().chars().count())
    }

    /// Apply one key press.
    fn key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        elevator: &mut dyn Elevator,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
    ) -> PromptOutcome {
        match key {
            Key::Named(NamedKey::Escape) => self.conclude(shell, compositor, None),
            Key::Named(NamedKey::Tab) => {
                if let Some(active) = self.active.as_mut() {
                    active.set_focus(active.focus.next());
                }
                self.repaint(shell, compositor);
                PromptOutcome::Pending
            }
            Key::Named(NamedKey::Enter) => {
                let Some(active) = self.active.as_ref() else {
                    return PromptOutcome::Pending;
                };
                match active.focus {
                    // A button holds the keyboard: it decides, not the fields.
                    Focus::Cancel => self.conclude(shell, compositor, None),
                    Focus::Account | Focus::Secret | Focus::Continue => {
                        self.offer(elevator, shell, compositor)
                    }
                }
            }
            other => {
                let scale = compositor.scale();
                let mut sink = damage::sink();
                if let Some(active) = self.active.as_mut() {
                    match active.focus {
                        Focus::Account => {
                            let _ = active.account.on_key(
                                other,
                                modifiers,
                                field_rect(scale, 0),
                                &mut sink,
                            );
                        }
                        Focus::Secret => {
                            let _ = active.secret.on_key(
                                other,
                                modifiers,
                                field_rect(scale, 1),
                                &mut sink,
                            );
                        }
                        // A focused button takes no text.
                        Focus::Cancel | Focus::Continue => {}
                    }
                }
                self.repaint(shell, compositor);
                PromptOutcome::Pending
            }
        }
    }

    /// Apply one pointer event: a press in a field moves the keyboard there
    /// and places the caret; a completed click on a button decides.
    fn pointer(
        &mut self,
        event: &InputEvent,
        elevator: &mut dyn Elevator,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
    ) -> PromptOutcome {
        let scale = compositor.scale();
        let theme = shell.session().active_theme().clone();
        let bounds = window_bounds(scale);
        let mut sink = damage::sink();
        let action = {
            let Some(active) = self.active.as_mut() else {
                return PromptOutcome::Pending;
            };
            if let InputEvent::PointerMoved { to } = event {
                active.pointer = *to;
            }
            if let InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } = event
            {
                if let Some(focus) = active.field_under_pointer(scale) {
                    active.set_focus(focus);
                }
            }
            let account_rect = field_rect(scale, 0);
            let secret_rect = field_rect(scale, 1);
            let _ = active
                .account
                .on_pointer(event, account_rect, scale, &theme, &mut sink);
            let _ = active
                .secret
                .on_pointer(event, secret_rect, scale, &theme, &mut sink);
            active
                .dialog
                .on_pointer(event, bounds, scale, &theme, &mut sink)
        };
        match action {
            Some(DialogAction::ActionActivated {
                index: CONTINUE_ACTION,
            }) => self.offer(elevator, shell, compositor),
            Some(DialogAction::ActionActivated {
                index: CANCEL_ACTION,
            }) => self.conclude(shell, compositor, None),
            // A band that reported some other index is not one of the two
            // buttons this prompt built; deciding nothing leaves the prompt
            // up rather than guessing what the user meant.
            _ => {
                self.repaint(shell, compositor);
                PromptOutcome::Pending
            }
        }
    }

    /// Offer what has been typed to the broker.
    ///
    /// An empty field is not offered at all: there is nothing to check, and
    /// asking would spend an audited attempt against the account. A refusal
    /// keeps the prompt up with the reason stated and the password cleared —
    /// zeroised as the field discards it — so another try starts from empty.
    fn offer(
        &mut self,
        elevator: &mut dyn Elevator,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
    ) -> PromptOutcome {
        let outcome = {
            let Some(active) = self.active.as_mut() else {
                return PromptOutcome::Pending;
            };
            if active.account.text().is_empty() || active.secret.text().is_empty() {
                active.focus_first_empty();
                None
            } else {
                Some(elevator.launch(active.account.text(), active.secret.text(), &active.program))
            }
        };
        match outcome {
            Some(Ok(pid)) => self.conclude(shell, compositor, Some(pid)),
            Some(Err(err)) => {
                if let Some(active) = self.active.as_mut() {
                    active.refuse(err);
                }
                self.repaint(shell, compositor);
                PromptOutcome::Pending
            }
            None => {
                self.repaint(shell, compositor);
                PromptOutcome::Pending
            }
        }
    }

    /// Close the prompt window and produce the outcome. `started` carries the
    /// pid of an accepted launch, or `None` for every path that offered
    /// nothing.
    fn conclude(
        &mut self,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        started: Option<i32>,
    ) -> PromptOutcome {
        let Some(active) = self.active.take() else {
            return PromptOutcome::Pending;
        };
        let _ = shell.close_window(compositor, active.wm);
        // Dropping `active` here zeroises both fields' buffers.
        match started {
            Some(pid) => PromptOutcome::Started { pid },
            None => PromptOutcome::Cancelled,
        }
    }
}

impl ActivePrompt {
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

    /// Move the keyboard to the first field that is still empty, so a
    /// premature submission lands where the user still has to type.
    fn focus_first_empty(&mut self) {
        let focus = if self.account.text().is_empty() {
            Focus::Account
        } else {
            Focus::Secret
        };
        self.set_focus(focus);
    }

    /// Which field the pointer is over, if either.
    fn field_under_pointer(&self, scale: Scale) -> Option<Focus> {
        if field_rect(scale, 0).contains(self.pointer) {
            return Some(Focus::Account);
        }
        field_rect(scale, 1)
            .contains(self.pointer)
            .then_some(Focus::Secret)
    }

    /// State a refusal and clear the password for another attempt.
    ///
    /// The account name is left as typed: it is not the secret, and retyping
    /// it would only make a correct name harder to keep.
    fn refuse(&mut self, err: Errno) {
        let reason = if err == Errno::PermissionDenied {
            REFUSED_REASON
        } else {
            NOT_STARTED_REASON
        };
        self.dialog = build_dialog_with_reason(self.dialog.message().unwrap_or_default(), reason);
        // Discarding the buffer zeroises it.
        self.secret.set_text("");
        self.set_focus(Focus::Secret);
    }
}

/// The prompt window's title.
const TITLE: &str = "Authenticate";

/// The account field's placeholder, naming what is wanted.
const ACCOUNT_LABEL: &str = "Account name";

/// The secret field's placeholder.
const SECRET_LABEL: &str = "Password";

/// The prompt window's own rectangle: its physical extent at its own origin,
/// which is where its pixels start.
fn window_bounds(scale: Scale) -> Rect {
    Rect::new(
        0,
        0,
        scale.scale_length(WIN_WIDTH),
        scale.scale_length(WIN_HEIGHT),
    )
}

/// The physical rectangle of field `index` (`0` account, `1` password) in the
/// prompt window's own space.
///
/// The one definition of the fields' geometry, so the paint, the hit test, and
/// the pointer routing all resolve the same rectangles rather than each
/// re-deriving them. Exported within the crate so a host-side test clicks
/// where the field actually is.
pub(crate) fn field_rect(scale: Scale, index: u32) -> Rect {
    let inset = scale.scale_length(FIELD_INSET);
    let top = scale.scale_length(FIELD_TOP + FIELD_PITCH * index);
    let width = scale
        .scale_length(WIN_WIDTH)
        .saturating_sub(inset.saturating_mul(2));
    Rect::new(
        i32::try_from(inset).unwrap_or(i32::MAX),
        i32::try_from(top).unwrap_or(i32::MAX),
        width,
        scale.scale_length(FIELD_HEIGHT),
    )
}

/// Build the prompt's dialog: the cancelling button leading, the continuing
/// one trailing, and neither focused — the keyboard starts in the account
/// field.
fn build_dialog(purpose: &str) -> Dialog {
    Dialog::new(TITLE)
        .with_message(purpose)
        .with_actions(alloc::vec![
            Button::labelled("Cancel"),
            Button::new(
                ButtonContent::Label(String::from("Continue")),
                ControlRole::Neutral,
            ),
        ])
}

/// The same dialog carrying an inline refusal reason.
fn build_dialog_with_reason(purpose: &str, reason: &str) -> Dialog {
    build_dialog(purpose).with_reason(reason)
}

/// Paint the prompt at the window's physical extents through the active theme:
/// the dialog chrome first, then the two fields over its body.
fn render_surface(
    dialog: &Dialog,
    account: &TextField,
    secret: &TextField,
    scale: Scale,
    shell: &DesktopShell,
) -> Option<Surface> {
    let theme = shell.session().active_theme();
    let bounds = window_bounds(scale);
    let mut surface = Surface::new(bounds.width, bounds.height)?;
    dialog.render(&mut surface, bounds, scale, theme);
    account.render(&mut surface, field_rect(scale, 0), scale, theme);
    secret.render(&mut surface, field_rect(scale, 1), scale, theme);
    Some(surface)
}
