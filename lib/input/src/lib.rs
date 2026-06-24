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
//! sibling userland crate. Per the shared
//! vocabulary therefore lives in `lib/*` — exactly the reasoning that placed
//! `Point`/`Rect` in [`rustos_geometry`] and the colour algebra in
//! `rustos_raster`. The window manager re-exports these types, so both the
//! compositor's [`InputRouter`] and the taskbar's input router consume one
//! definition.
//!
//! Keyboard input is modelled alongside the pointer: a [`Key`] (a produced
//! character or a [`NamedKey`]) and the [`Modifiers`] held with it travel as
//! the [`InputEvent::KeyPressed`] / [`InputEvent::KeyReleased`] variants the
//! window manager delivers to the focused surface. This is the in-process
//! routing vocabulary; the bytes that cross the kernel boundary are
//! `rustos_abi`'s `KeyInput`, the same producer/consumer split as the pointer
//! ([`PointerButton`] vs `rustos_abi`'s `PointerButtonCode`).
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

/// The keyboard modifiers held while a key event was produced.
//
// The four booleans are independent modifier-key states, not a state
// machine: any combination is legal, so a flat record models them more
// clearly than an enum.
#[allow(clippy::struct_excessive_bools)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Modifiers {
    /// The shift key was held.
    pub shift: bool,
    /// The control key was held.
    pub ctrl: bool,
    /// The alt key was held.
    pub alt: bool,
    /// The meta (super / command) key was held.
    pub meta: bool,
}

/// A named non-character key the desktop distinguishes.
///
/// Character-producing keys are not listed here: they arrive as a
/// [`Key::Char`]. This is the closed set of keys that produce no character,
/// matching `rustos_abi`'s wire `NamedKeyCode`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NamedKey {
    /// The Enter / Return key.
    Enter,
    /// The Escape key.
    Escape,
    /// The Backspace key.
    Backspace,
    /// The Tab key.
    Tab,
    /// The forward-delete key.
    Delete,
    /// The Insert key.
    Insert,
    /// The Home key.
    Home,
    /// The End key.
    End,
    /// The Page Up key.
    PageUp,
    /// The Page Down key.
    PageDown,
    /// The left-arrow key.
    Left,
    /// The right-arrow key.
    Right,
    /// The up-arrow key.
    Up,
    /// The down-arrow key.
    Down,
    /// A function key, `F1` through `F12` (`number` in `1..=12`).
    Function {
        /// The function-key number, `1` through `12`.
        number: u8,
    },
}

/// A key the desktop routes: either a produced character or a [`NamedKey`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Key {
    /// A character-producing key, carrying the produced Unicode scalar.
    Char(char),
    /// A named non-character key.
    Named(NamedKey),
}

/// A device-level input event delivered to a desktop input router.
///
/// Pointer button events act at the pointer's current position; that position
/// is updated only by [`InputEvent::PointerMoved`], exactly as a real pointing
/// device reports motion separately from clicks. A router therefore tracks
/// the latest position itself and applies presses and releases there. Key
/// events are delivered to whichever surface currently holds the keyboard
/// focus, which the router tracks independently of the pointer.
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
    /// A key was pressed; it is delivered to the focused surface.
    KeyPressed {
        /// The key that went down.
        key: Key,
        /// The modifiers held while it was pressed.
        modifiers: Modifiers,
    },
    /// A key was released; it is delivered to the focused surface.
    KeyReleased {
        /// The key that came up.
        key: Key,
        /// The modifiers held while it was released.
        modifiers: Modifiers,
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

    #[test]
    fn key_events_carry_key_and_modifiers() {
        let event = InputEvent::KeyPressed {
            key: Key::Char('x'),
            modifiers: Modifiers {
                ctrl: true,
                ..Modifiers::default()
            },
        };
        match event {
            InputEvent::KeyPressed { key, modifiers } => {
                assert_eq!(key, Key::Char('x'));
                assert!(modifiers.ctrl && !modifiers.shift);
            }
            other => panic!("expected a key press, got {other:?}"),
        }
    }

    #[test]
    fn named_keys_are_distinct_from_characters() {
        assert_ne!(
            Key::Named(NamedKey::Enter),
            Key::Named(NamedKey::Function { number: 1 })
        );
        assert_ne!(Key::Char('\n'), Key::Named(NamedKey::Enter));
    }
}
