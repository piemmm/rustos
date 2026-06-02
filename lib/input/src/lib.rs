//! RustOS shared pointer input-event vocabulary (`lib/input`).
//!
//! This crate owns the device-level pointer event types the desktop routes:
//! the [`PointerButton`]s it distinguishes and the [`InputEvent`]s a pointing
//! device reports (motion, press, release). They are pure data with no
//! window-manager or taskbar dependency.
//!
//! # Where it sits
//!
//! These types were defined inside `userland/gui/wm`, but the taskbar must
//! route the *same* pointer events to hit-test its regions, and a
//! `userland/gui/*` crate may not depend on the window manager nor on a
//! sibling userland crate (`AGENTS.md` §17.4). Per §6 / §2.2 the shared
//! vocabulary therefore lives in `lib/*` — exactly the reasoning that placed
//! `Point`/`Rect` in [`rustos_geometry`] and the colour algebra in
//! `rustos_raster`. The window manager re-exports these types, so both the
//! compositor's [`InputRouter`] and the taskbar's input router consume one
//! definition.
//!
//! Keyboard input is deliberately **not** modelled here: the desktop tracks
//! *which* surface owns the keyboard, but the key encoding is a separate ABI
//! concern that is not invented in this layer (`AGENTS.md` §2.4 — no
//! interface creep).
//!
//! [`InputRouter`]: https://docs.rs/rustos-wm
//! [`rustos_geometry`]: rustos_geometry

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use rustos_geometry::Point;

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

/// A device-level pointer event delivered to a desktop input router.
///
/// Button events act at the pointer's current position; that position is
/// updated only by [`InputEvent::PointerMoved`], exactly as a real pointing
/// device reports motion separately from clicks. A router therefore tracks
/// the latest position itself and applies presses and releases there.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_are_distinct() {
        assert_ne!(PointerButton::Primary, PointerButton::Secondary);
        assert_ne!(PointerButton::Secondary, PointerButton::Middle);
        assert_ne!(PointerButton::Primary, PointerButton::Middle);
    }

    #[test]
    fn pointer_moved_carries_its_position() {
        let event = InputEvent::PointerMoved {
            to: Point::new(7, 11),
        };
        match event {
            InputEvent::PointerMoved { to } => assert_eq!(to, Point::new(7, 11)),
            other => panic!("expected a move, got {other:?}"),
        }
    }

    #[test]
    fn press_and_release_name_their_button() {
        let pressed = InputEvent::PointerPressed {
            button: PointerButton::Primary,
        };
        let released = InputEvent::PointerReleased {
            button: PointerButton::Secondary,
        };
        assert_eq!(
            pressed,
            InputEvent::PointerPressed {
                button: PointerButton::Primary
            }
        );
        assert_ne!(pressed, released);
    }

    #[test]
    fn events_are_copy() {
        let event = InputEvent::PointerPressed {
            button: PointerButton::Middle,
        };
        let copy = event;
        assert_eq!(event, copy);
    }
}
