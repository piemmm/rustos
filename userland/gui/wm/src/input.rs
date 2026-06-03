//! Input routing: focus, click-to-activate, and interactive move-grabs.
//!
//! The [`InputRouter`] turns a stream of device-level pointer
//! [`InputEvent`]s into window-manager actions against a
//! [`Compositor`]: it tracks the pointer position and the focused
//! window, raises and focuses the window under a primary-button press
//! (*click-to-activate*), and drives an interactive *move-grab* so a
//! window can be dragged across the screen.
//!
//! The router holds no pixels and performs no compositing; it is the
//! policy layer that sits on top of the compositor's scene graph. A
//! move-grab is started explicitly through [`InputRouter::begin_move`]
//! rather than armed on every press, so a press that lands on a
//! window's *content* (reported back to the client through
//! [`InputResponse::Activated`]) never moves the window — only a grab
//! the desktop decorations choose to start, typically when the press
//! landed on a title bar, does. This keeps content interaction and
//! window dragging cleanly separated (`AGENTS.md` §2.1 — no "drag
//! anywhere" hack).
//!
//! Keyboard input is delivered to the focused window: the router owns
//! *which* window has the keyboard ([`InputRouter::focused`]) and routes a
//! [`InputEvent::KeyPressed`] / [`InputEvent::KeyReleased`] to it as an
//! [`InputResponse::Key`], leaving the bytes-on-the-wire encoding to
//! `rustos_abi`'s `KeyInput` (`AGENTS.md` §9). A key with no focused window
//! (focus on the desktop, or the focused window since gone) is ignored
//! rather than misdelivered (`AGENTS.md` §2.9).

use crate::geometry::Point;
use crate::window::{Window, WindowId};
use crate::Compositor;

// The device-level pointer vocabulary the router consumes is shared with the
// taskbar, which routes the same events but may not depend on the window
// manager (`AGENTS.md` §17.4). It therefore lives in `lib/input`; the
// compositor re-exports it so callers keep referring to
// `rustos_wm::{InputEvent, PointerButton}` (§2.2 — one definition).
pub use rustos_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};

/// What the [`InputRouter`] did with an [`InputEvent`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum InputResponse {
    /// The event changed no window-manager state.
    Ignored,
    /// A primary press activated `window`: it was raised to the top of
    /// the z-order and given focus. `local` is the press position in
    /// that window's surface coordinates, for the client to interpret.
    Activated {
        /// The activated window.
        window: WindowId,
        /// Press position relative to the window's top-left corner.
        local: Point,
    },
    /// A primary press landed on the desktop background; focus, if any,
    /// was cleared.
    DesktopPressed,
    /// An active move-grab dragged `window` to a new `origin`.
    Moved {
        /// The window being dragged.
        window: WindowId,
        /// The window's new top-left position.
        origin: Point,
    },
    /// An active move-grab ended (primary released, or the grabbed
    /// window vanished).
    MoveEnded {
        /// The window whose move-grab ended.
        window: WindowId,
    },
    /// A key event was delivered to the focused `window` for the client to
    /// interpret. The router takes no action of its own on a key; it only
    /// names the recipient.
    Key {
        /// The window that holds the keyboard and received the event.
        window: WindowId,
        /// The key that changed state.
        key: Key,
        /// The modifiers held while the key changed state.
        modifiers: Modifiers,
        /// `true` for a press, `false` for a release.
        pressed: bool,
    },
}

/// An in-flight interactive move: the grabbed window and the offset
/// from the pointer to the window's top-left corner, held constant for
/// the duration of the drag so the window tracks the pointer without
/// jumping.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct MoveGrab {
    window: WindowId,
    offset: Point,
}

/// Routes pointer input into [`Compositor`] actions.
///
/// The router is the desktop's input-policy state: the current pointer
/// position, the focused window, and any in-flight move-grab. It is
/// driven entirely through [`InputRouter::handle`] and
/// [`InputRouter::begin_move`]; it never panics and never grants
/// itself authority over a window it was not handed by the compositor.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InputRouter {
    pointer: Point,
    focused: Option<WindowId>,
    grab: Option<MoveGrab>,
}

impl InputRouter {
    /// Create a router with the pointer at the screen origin, no focus,
    /// and no active grab.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The current pointer position in screen coordinates.
    #[must_use]
    pub const fn pointer(&self) -> Point {
        self.pointer
    }

    /// The focused window, the one that receives keyboard input, or
    /// `None` when focus rests on the desktop.
    #[must_use]
    pub const fn focused(&self) -> Option<WindowId> {
        self.focused
    }

    /// `true` while an interactive move-grab is in progress.
    #[must_use]
    pub const fn is_moving(&self) -> bool {
        self.grab.is_some()
    }

    /// Give keyboard focus to `window`, validated against `compositor`.
    ///
    /// This is the programmatic counterpart of focusing by a pointer press:
    /// the taskbar's running-task list activates a window by id rather than by
    /// position, so the session glue moves focus here to keep the window
    /// manager's focus in step with the bar. Unlike a press it does not raise
    /// the window — the caller raises it (and shows it) so the policy stays in
    /// one place.
    ///
    /// Returns `false`, changing nothing, when `compositor` does not know
    /// `window` (`AGENTS.md` §2.9 — fail closed; the router never grants itself
    /// focus over a window it was not handed).
    pub fn focus(&mut self, window: WindowId, compositor: &Compositor) -> bool {
        if compositor.window(window).is_none() {
            return false;
        }
        self.focused = Some(window);
        true
    }

    /// Drop keyboard focus, leaving it on the desktop.
    ///
    /// The session glue calls this when the focused window is minimised from
    /// the taskbar: the window is hidden, so nothing should hold the keyboard.
    pub fn unfocus(&mut self) {
        self.focused = None;
    }

    /// Process one input `event` against `compositor`, returning what
    /// changed.
    pub fn handle(&mut self, event: InputEvent, compositor: &mut Compositor) -> InputResponse {
        match event {
            InputEvent::PointerMoved { to } => {
                self.pointer = to;
                self.drag_to(to, compositor)
            }
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => self.press_primary(compositor),
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => self.release_primary(),
            InputEvent::PointerPressed { .. } | InputEvent::PointerReleased { .. } => {
                InputResponse::Ignored
            }
            InputEvent::KeyPressed { key, modifiers } => {
                self.deliver_key(key, modifiers, true, compositor)
            }
            InputEvent::KeyReleased { key, modifiers } => {
                self.deliver_key(key, modifiers, false, compositor)
            }
        }
    }

    /// Deliver a key event to the focused window, naming it as the recipient.
    ///
    /// Returns [`InputResponse::Ignored`] when focus rests on the desktop or
    /// the focused window is no longer known to `compositor` — in the latter
    /// case focus is dropped so a stale window never keeps the keyboard
    /// (`AGENTS.md` §2.9 — fail closed).
    fn deliver_key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        pressed: bool,
        compositor: &Compositor,
    ) -> InputResponse {
        let Some(window) = self.focused else {
            return InputResponse::Ignored;
        };
        if compositor.window(window).is_none() {
            self.focused = None;
            return InputResponse::Ignored;
        }
        InputResponse::Key {
            window,
            key,
            modifiers,
            pressed,
        }
    }

    /// Start an interactive move-grab on the focused window, anchored at
    /// the current pointer position. Returns `false` when there is no
    /// focused window or it is no longer known to the compositor, in
    /// which case no grab is started (`AGENTS.md` §2.9 — fail closed).
    ///
    /// Decorations call this when a press lands on a window's move
    /// handle (e.g. its title bar); the subsequent pointer motion then
    /// drags the window through [`InputResponse::Moved`].
    pub fn begin_move(&mut self, compositor: &Compositor) -> bool {
        let Some(window) = self.focused else {
            return false;
        };
        let Some(origin) = compositor.window(window).map(Window::origin) else {
            return false;
        };
        self.grab = Some(MoveGrab {
            window,
            offset: Point::new(
                self.pointer.x.saturating_sub(origin.x),
                self.pointer.y.saturating_sub(origin.y),
            ),
        });
        true
    }

    /// Handle a primary-button press at the current pointer position.
    fn press_primary(&mut self, compositor: &mut Compositor) -> InputResponse {
        let Some(window) = compositor.window_at(self.pointer) else {
            self.focused = None;
            return InputResponse::DesktopPressed;
        };
        compositor.raise(window);
        self.focused = Some(window);
        let origin = compositor
            .window(window)
            .map_or(Point::ORIGIN, Window::origin);
        InputResponse::Activated {
            window,
            local: Point::new(
                self.pointer.x.saturating_sub(origin.x),
                self.pointer.y.saturating_sub(origin.y),
            ),
        }
    }

    /// Handle a primary-button release: end any active move-grab.
    fn release_primary(&mut self) -> InputResponse {
        match self.grab.take() {
            Some(grab) => InputResponse::MoveEnded {
                window: grab.window,
            },
            None => InputResponse::Ignored,
        }
    }

    /// Apply pointer motion to an active move-grab, if any.
    fn drag_to(&mut self, to: Point, compositor: &mut Compositor) -> InputResponse {
        let Some(grab) = self.grab else {
            return InputResponse::Ignored;
        };
        let origin = Point::new(
            to.x.saturating_sub(grab.offset.x),
            to.y.saturating_sub(grab.offset.y),
        );
        if compositor.move_window(grab.window, origin) {
            InputResponse::Moved {
                window: grab.window,
                origin,
            }
        } else {
            self.grab = None;
            InputResponse::MoveEnded {
                window: grab.window,
            }
        }
    }
}
