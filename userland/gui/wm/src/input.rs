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
//! Keyboard input is not modelled here: the router owns *which* window
//! has the keyboard ([`InputRouter::focused`]); the key encoding is a
//! separate ABI concern and is not invented in the compositor
//! (`AGENTS.md` §2.4 — no interface creep).

use crate::geometry::Point;
use crate::window::{Window, WindowId};
use crate::Compositor;

/// The pointer buttons the desktop distinguishes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PointerButton {
    /// The primary (typically left) button: activates and grabs.
    Primary,
    /// The secondary (typically right) button: context actions.
    Secondary,
    /// The middle button.
    Middle,
}

/// A device-level input event delivered to the [`InputRouter`].
///
/// Button events act at the pointer's current position; that position
/// is updated only by [`InputEvent::PointerMoved`], exactly as a real
/// pointing device reports motion separately from clicks.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum InputEvent {
    /// The pointer moved to an absolute screen position.
    PointerMoved {
        /// New pointer position, in screen coordinates.
        to: Point,
    },
    /// A pointer button was pressed at the current pointer position.
    PointerPressed {
        /// The button that went down.
        button: PointerButton,
    },
    /// A pointer button was released at the current pointer position.
    PointerReleased {
        /// The button that came up.
        button: PointerButton,
    },
}

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
