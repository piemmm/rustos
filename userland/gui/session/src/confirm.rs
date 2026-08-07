//! The desktop session's **trusted confirmation prompt** for a system
//! command that cannot be undone.
//!
//! Restarting or powering the machine off ends every task of every principal
//! on it, so neither may happen on a single click. The session — not the
//! taskbar, which holds no authority at all — puts the choice to the user in
//! a window it owns, drawn with the shared `lib/controls` dialog, and only a
//! deliberate confirmation produces the outcome the caller relays onward.
//!
//! The prompt is modelled on the session's trusted file picker: one slot (a
//! second request while one is showing is refused rather than stacking a
//! second prompt), a session-owned compositor window, and a typed conclusion
//! the embedder acts on once the window is already closed.
//!
//! Every path that is not an explicit confirmation — the safe button,
//! `Escape`, or a request the session had to abandon — concludes as a
//! cancellation, so nothing irreversible can follow from a prompt the user
//! did not answer.

use tairix_abi::input::{KeyInput, KeyValue, NamedKeyCode};
use tairix_abi::PowerAction;
use tairix_controls::{
    Button, ControlRole, ControlState, Dialog, DialogAction, FocusState, PointerState,
};
use tairix_geometry::Scale;
use tairix_wm::{
    Compositor, InputEvent, Key, NamedKey, Point, PointerButton, Rect, Surface, WindowId,
};

use crate::shell::DesktopShell;

/// The prompt window's width in pixels.
///
/// Wide enough for the longest confirmation sentence on one line at the
/// reference density, narrow enough to read as a question rather than a
/// document.
pub const WIN_WIDTH: u32 = 460;

/// The prompt window's height in pixels: the title, one sentence of body
/// text, and the action band beneath them.
pub const WIN_HEIGHT: u32 = 170;

/// Top-left of the prompt window, in screen pixels.
///
/// One deterministic spot, clear of the first window-cascade slots, exported
/// so a host-side observer drives the prompt where the session actually puts
/// it rather than at a re-derived guess.
pub const CONFIRM_ORIGIN: Point = Point::new(240, 200);

/// Index of the safe button in the dialog's action band.
///
/// Leading, and the one that holds keyboard focus when the prompt opens, so
/// the answer a stray `Enter` gives is "no".
const CANCEL_ACTION: usize = 0;

/// Index of the confirming button.
const CONFIRM_ACTION: usize = 1;

/// How the user answered a confirmation prompt.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Answer {
    /// The user confirmed: the caller may now relay the transition.
    Confirmed(PowerAction),
    /// The user declined, dismissed the prompt, or it was abandoned.
    /// Nothing is relayed.
    Declined,
}

/// One showing prompt: what it is asking about, its compositor window, and
/// the shared dialog behind it.
struct ActivePrompt {
    action: PowerAction,
    wm: WindowId,
    dialog: Dialog,
}

/// The session's confirmation-prompt slot.
///
/// Idle until a destructive command is chosen, then holding exactly one
/// prompt until the user answers it.
#[derive(Default)]
pub struct ConfirmPrompt {
    active: Option<ActivePrompt>,
}

impl ConfirmPrompt {
    /// An idle prompt.
    #[must_use]
    pub const fn new() -> Self {
        Self { active: None }
    }

    /// The compositor window of the showing prompt, if one is up.
    ///
    /// The embedder routes this window's key and click input into
    /// [`handle_key`](Self::handle_key) / [`handle_click`](Self::handle_click)
    /// rather than to any served window.
    #[must_use]
    pub fn wm_id(&self) -> Option<WindowId> {
        self.active.as_ref().map(|active| active.wm)
    }

    /// The transition the showing prompt is asking about, if one is up.
    #[must_use]
    pub fn pending(&self) -> Option<PowerAction> {
        self.active.as_ref().map(|active| active.action)
    }

    /// Ask the user to confirm `action`.
    ///
    /// Returns whether the prompt came up. A prompt already showing, or a
    /// window the compositor could not give, answers `false`: the caller
    /// relays nothing, so a prompt that cannot be shown can never be taken
    /// for an answer.
    pub fn ask(
        &mut self,
        action: PowerAction,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
    ) -> bool {
        if self.active.is_some() {
            return false;
        }
        let dialog = build_dialog(action);
        let Some(surface) = render_surface(&dialog, compositor.scale(), shell) else {
            return false;
        };
        let Some(wm) = shell.open_window(compositor, CONFIRM_ORIGIN, surface, title(action)) else {
            return false;
        };
        self.active = Some(ActivePrompt { action, wm, dialog });
        true
    }

    /// Apply one key press to the showing prompt.
    ///
    /// `Escape` declines outright. `Enter`/`Space` activate whichever button
    /// holds focus, which is the safe one when the prompt opens, so
    /// confirming always takes a deliberate move to the other button.
    ///
    /// Returns the answer once the user has given one; the prompt window is
    /// already closed by then.
    pub fn handle_key(
        &mut self,
        key: &KeyInput,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
    ) -> Option<Answer> {
        let KeyInput::Pressed { key, .. } = key else {
            return None;
        };
        match key {
            KeyValue::Named(NamedKeyCode::Escape) => self.conclude(shell, compositor, false),
            KeyValue::Named(NamedKeyCode::Tab | NamedKeyCode::Left | NamedKeyCode::Right) => {
                self.move_focus(shell, compositor);
                None
            }
            KeyValue::Named(NamedKeyCode::Enter) => {
                let action = {
                    let active = self.active.as_mut()?;
                    active.dialog.on_key(Key::Named(NamedKey::Enter))
                };
                self.resolve(action, shell, compositor)
            }
            _ => None,
        }
    }

    /// Apply one primary-button press at the prompt-window-local position
    /// `local`.
    ///
    /// A press-and-release on a button answers with it; a press anywhere
    /// else inside the prompt changes nothing and leaves the question up, so
    /// the transition follows only from pressing the confirming button.
    pub fn handle_click(
        &mut self,
        local: Point,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
    ) -> Option<Answer> {
        let scale = compositor.scale();
        let w = scale.scale_length(WIN_WIDTH);
        let h = scale.scale_length(WIN_HEIGHT);
        let bounds = Rect::new(0, 0, w, h);
        let theme = shell.session().active_theme().clone();
        let action = {
            let active = self.active.as_mut()?;
            // A button resolves on the completed click, so the press and the
            // release are both fed at the same point the user pressed.
            let _ = active.dialog.on_pointer(
                &InputEvent::PointerMoved { to: local },
                bounds,
                scale,
                &theme,
            );
            let _ = active.dialog.on_pointer(
                &InputEvent::PointerPressed {
                    button: PointerButton::Primary,
                },
                bounds,
                scale,
                &theme,
            );
            active.dialog.on_pointer(
                &InputEvent::PointerReleased {
                    button: PointerButton::Primary,
                },
                bounds,
                scale,
                &theme,
            )
        };
        self.resolve(action, shell, compositor)
    }

    /// Take the prompt down without an answer, declining the transition.
    ///
    /// Used when the session is tearing the desktop down or the theme it was
    /// drawn against is gone: an unanswered prompt is never a confirmation.
    pub fn abandon(&mut self, shell: &mut DesktopShell, compositor: &mut Compositor) {
        let _ = self.conclude(shell, compositor, false);
    }

    /// Repaint the showing prompt, so a theme switch behind it redraws it in
    /// the appearance now in use. A surface that cannot be built leaves the
    /// previous frame up rather than failing.
    pub fn repaint(&mut self, shell: &mut DesktopShell, compositor: &mut Compositor) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        if let Some(surface) = render_surface(&active.dialog, compositor.scale(), shell) {
            let _ = compositor.set_surface(active.wm, surface);
        }
    }

    /// Turn a dialog action into an answer, ignoring anything that is not one
    /// of the prompt's two buttons.
    fn resolve(
        &mut self,
        action: Option<DialogAction>,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
    ) -> Option<Answer> {
        match action {
            Some(DialogAction::ActionActivated {
                index: CONFIRM_ACTION,
            }) => self.conclude(shell, compositor, true),
            Some(DialogAction::ActionActivated {
                index: CANCEL_ACTION,
            }) => self.conclude(shell, compositor, false),
            // A band that reported some other index is not one of the two
            // buttons this prompt built; answering nothing leaves the prompt
            // up rather than guessing which way the user meant it.
            _ => None,
        }
    }

    /// Move keyboard focus to the other button and repaint, so the focus ring
    /// the user is steering by is the one on screen.
    fn move_focus(&mut self, shell: &mut DesktopShell, compositor: &mut Compositor) {
        if let Some(active) = self.active.as_mut() {
            let focused = focused_index(&active.dialog);
            let next = usize::from(focused == Some(CANCEL_ACTION));
            set_focus(&mut active.dialog, next);
        }
        self.repaint(shell, compositor);
    }

    /// Close the prompt window and produce the answer.
    fn conclude(
        &mut self,
        shell: &mut DesktopShell,
        compositor: &mut Compositor,
        confirmed: bool,
    ) -> Option<Answer> {
        let active = self.active.take()?;
        let _ = shell.close_window(compositor, active.wm);
        Some(if confirmed {
            Answer::Confirmed(active.action)
        } else {
            Answer::Declined
        })
    }
}

/// The prompt window's title for `action`.
const fn title(action: PowerAction) -> &'static str {
    match action {
        PowerAction::PowerOff => "Shut Down",
        PowerAction::Restart => "Restart",
    }
}

/// What the prompt says will happen, in plain words, so the user is agreeing
/// to a consequence rather than to a verb.
const fn message(action: PowerAction) -> &'static str {
    match action {
        PowerAction::PowerOff => {
            "Every open application will be closed and the machine will switch off."
        }
        PowerAction::Restart => {
            "Every open application will be closed and the machine will start again."
        }
    }
}

/// The label on the confirming button: the same words as the row that asked,
/// so the user confirms the thing they chose.
const fn confirm_label(action: PowerAction) -> &'static str {
    title(action)
}

/// Build the prompt's dialog: the safe button leading and focused, the
/// destructive one trailing.
///
/// The one definition of the prompt's shape, so the render, the hit-test,
/// and the button geometry a caller resolves through
/// [`Dialog::action_rects`] all agree rather than each re-deriving it.
pub(crate) fn build_dialog(action: PowerAction) -> Dialog {
    let mut cancel = Button::labelled("Cancel");
    cancel.set_state(ControlState::default().with_focus(FocusState::FOCUSED));
    let confirm = Button::new(
        tairix_controls::ButtonContent::Label(alloc::string::String::from(confirm_label(action))),
        ControlRole::Destructive,
    );
    Dialog::new(title(action))
        .with_message(message(action))
        .with_actions(alloc::vec![cancel, confirm])
}

/// Which action button currently holds keyboard focus, if any.
fn focused_index(dialog: &Dialog) -> Option<usize> {
    dialog
        .actions()
        .iter()
        .position(|button| button.state().focus.focused)
}

/// Give keyboard focus to the button at `index` and take it from the others,
/// so exactly one focus ring is drawn and `Enter` can only mean one thing.
fn set_focus(dialog: &mut Dialog, index: usize) {
    for (position, button) in dialog.actions_mut().iter_mut().enumerate() {
        let focus = if position == index {
            FocusState::FOCUSED
        } else {
            FocusState::default()
        };
        button.set_state(
            button
                .state()
                .with_focus(focus)
                .with_pointer(PointerState::None),
        );
    }
}

/// Paint the prompt at the window's physical extents through the active theme.
fn render_surface(dialog: &Dialog, scale: Scale, shell: &DesktopShell) -> Option<Surface> {
    let theme = shell.session().active_theme();
    let w = scale.scale_length(WIN_WIDTH);
    let h = scale.scale_length(WIN_HEIGHT);
    let mut surface = Surface::new(w, h)?;
    dialog.render(&mut surface, Rect::new(0, 0, w, h), scale, theme);
    Some(surface)
}
