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
//! What the prompt *asks* is the shared [`CredentialSheet`]: the desktop has
//! one credential surface, and the settings application composes the same
//! one inside its own window. This module owns only what is the session's —
//! the window the sheet is drawn in, the one-slot rule (a second request
//! while one is showing is refused rather than stacking a second prompt),
//! and the typed conclusion the embedder acts on once the window is already
//! closed.
//!
//! # The secret
//!
//! The password is held only in the sheet's masked field, which bounds its
//! buffer so it can never reallocate while filling and zeroises every byte
//! it discards — including on drop. The prompt therefore leaves no plaintext
//! behind on any exit: a cancellation, a refusal that clears the field for
//! another try, a successful launch, or the session being torn down around
//! it. Neither the offered password nor the account name reaches the system
//! log; only the broker audits the attempt.
//!
//! # Refusals
//!
//! A refused attempt leaves the prompt up with the reason stated and the
//! password cleared, so the user can try again without the surface
//! pretending anything happened. The broker refuses a wrong password, an
//! unknown account, and a locked account indistinguishably, and this prompt
//! repeats exactly what it was told rather than guessing which it was.

use alloc::string::String;

use tairix_abi::input::KeyInput;
use tairix_abi::Errno;
use tairix_controls::{
    damage, CredentialAction, CredentialSheet, CREDENTIAL_NOT_STARTED_REASON,
    CREDENTIAL_REFUSED_REASON,
};
use tairix_geometry::Scale;
use tairix_wm::{Compositor, InputEvent, Point, PointerButton, Rect, Surface, WindowId};

use crate::shell::DesktopShell;

/// The prompt window's width in logical pixels: the sheet's own.
pub use tairix_controls::CREDENTIAL_WIDTH as WIN_WIDTH;

/// The prompt window's height in logical pixels: the sheet's own.
pub use tairix_controls::CREDENTIAL_HEIGHT as WIN_HEIGHT;

/// Top-left of the prompt window, in screen pixels.
///
/// One deterministic spot, clear of the first window-cascade slots and of the
/// confirmation prompt's, exported so a host-side observer drives the prompt
/// where the session actually puts it rather than at a re-derived guess.
pub const ELEVATE_ORIGIN: Point = Point::new(280, 160);

/// One-shot: the credential prompt is on screen and holding the keyboard.
///
/// Emitted when [`ElevatePrompt::ask`] successfully opens the window, so a
/// host that must type into the fields can wait for a real surface rather
/// than racing the click that asked for it.
pub const ELEVATE_PROMPT_SHOWN: tairix_log::EventId = tairix_log::EventId(20_004);

/// The exact message [`ELEVATE_PROMPT_SHOWN`] is emitted with. A log
/// consumer keys on this rendered text, so it is defined once beside the
/// id and imported by both sides.
pub const ELEVATE_PROMPT_SHOWN_MESSAGE: &str = "credential prompt on screen";

/// What the prompt states when the broker refused the attempt.
pub use tairix_controls::CREDENTIAL_REFUSED_REASON as REFUSED_REASON;

/// What the prompt states when the account authenticated but the program did
/// not start.
pub use tairix_controls::CREDENTIAL_NOT_STARTED_REASON as NOT_STARTED_REASON;

/// The prompt window's title.
const TITLE: &str = "Authenticate";

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
    fn launch(&mut self, username: &str, password: &str, program: &str) -> Result<i64, Errno>;
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
        pid: i64,
    },
    /// The user cancelled, dismissed, or the prompt was abandoned. Nothing
    /// was started and nothing was offered.
    Cancelled,
}

/// One showing prompt: the program it will start, its compositor window, and
/// the sheet asking for the account.
struct ActivePrompt {
    program: String,
    wm: WindowId,
    sheet: CredentialSheet,
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
        let sheet = CredentialSheet::new(TITLE, purpose);
        let Some(surface) = render_surface(&sheet, compositor.scale(), shell) else {
            return false;
        };
        let Some(wm) = shell.open_window(compositor, ELEVATE_ORIGIN, surface, TITLE) else {
            return false;
        };
        self.active = Some(ActivePrompt {
            program: String::from(program),
            wm,
            sheet,
        });
        true
    }

    /// Apply one input event — pointer or key — to the showing prompt.
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
        let scale = compositor.scale();
        let theme = shell.session().active_theme().clone();
        let mut sink = damage::sink();
        let acted = {
            let Some(active) = self.active.as_mut() else {
                return PromptOutcome::Pending;
            };
            active
                .sheet
                .handle(event, window_bounds(scale), scale, &theme, &mut sink)
        };
        match acted {
            Some(CredentialAction::Cancelled) => self.conclude(shell, compositor, None),
            Some(CredentialAction::Offered) => self.offer(elevator, shell, compositor),
            None => {
                self.repaint(shell, compositor);
                PromptOutcome::Pending
            }
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
    /// drawn against is gone. The secret goes with the sheet, which zeroises
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
        if let Some(surface) = render_surface(&active.sheet, compositor.scale(), shell) {
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
            .and_then(|active| active.sheet.stated_reason())
    }

    /// The account name typed into the showing prompt. Host-test observation
    /// only.
    #[cfg(test)]
    pub(crate) fn account_text(&self) -> Option<&str> {
        self.active.as_ref().map(|active| active.sheet.account())
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
            .map_or(0, |active| active.sheet.secret().chars().count())
    }

    /// Offer what has been typed to the broker.
    ///
    /// A refusal keeps the prompt up with the reason stated and the password
    /// cleared — zeroised as the field discards it — so another try starts
    /// from empty.
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
            elevator.launch(
                active.sheet.account(),
                active.sheet.secret(),
                &active.program,
            )
        };
        match outcome {
            Ok(pid) => self.conclude(shell, compositor, Some(pid)),
            Err(err) => {
                if let Some(active) = self.active.as_mut() {
                    active.sheet.refuse(refusal(err));
                }
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
        started: Option<i64>,
    ) -> PromptOutcome {
        let Some(active) = self.active.take() else {
            return PromptOutcome::Pending;
        };
        let _ = shell.close_window(compositor, active.wm);
        // Dropping `active` here zeroises the sheet's fields.
        match started {
            Some(pid) => PromptOutcome::Started { pid },
            None => PromptOutcome::Cancelled,
        }
    }
}

/// What a refusal says: an authentication the broker would not accept, or a
/// program that would not start once it had.
///
/// Telling the two apart is the whole point — a user told to check a
/// password that was in fact accepted will only get it wrong again.
const fn refusal(err: Errno) -> &'static str {
    if matches!(err, Errno::PermissionDenied) {
        CREDENTIAL_REFUSED_REASON
    } else {
        CREDENTIAL_NOT_STARTED_REASON
    }
}

/// The prompt window's own rectangle: its physical extent at its own origin,
/// which is where its pixels start and therefore where the sheet is drawn.
fn window_bounds(scale: Scale) -> Rect {
    Rect::new(
        0,
        0,
        scale.scale_length(WIN_WIDTH),
        scale.scale_length(WIN_HEIGHT),
    )
}

/// Paint the sheet at the window's physical extents through the active
/// theme.
fn render_surface(sheet: &CredentialSheet, scale: Scale, shell: &DesktopShell) -> Option<Surface> {
    let theme = shell.session().active_theme();
    let bounds = window_bounds(scale);
    let mut surface = Surface::new(bounds.width, bounds.height)?;
    sheet.render(&mut surface, bounds, scale, theme);
    Some(surface)
}
