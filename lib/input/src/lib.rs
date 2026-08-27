//! TAIRiX shared pointer input-event vocabulary (`lib/input`).
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
//! `Point`/`Rect` in [`tairix_geometry`] and the colour algebra in
//! `tairix_raster`. The window manager re-exports these types, so both the
//! compositor's [`InputRouter`] and the taskbar's input router consume one
//! definition.
//!
//! # Device events and the seat's own answers
//!
//! [`InputEvent`] is strictly what a *device* reported. Which surface a
//! pointer event belongs to is a different kind of fact: it is derived from
//! the window stack, which only the desktop's seat can see, and it is carried
//! by [`PointerFocus`] rather than smuggled into the device vocabulary. The
//! two together are the whole pointer contract every router here obeys —
//! a router acts on the events it is handed, and is told when the pointer
//! stops resting on it.
//!
//! Keyboard input is modelled alongside the pointer: a [`Key`] (a produced
//! character or a [`NamedKey`]) and the [`Modifiers`] held with it travel as
//! the [`InputEvent::KeyPressed`] / [`InputEvent::KeyReleased`] variants the
//! window manager delivers to the focused surface. This is the in-process
//! routing vocabulary; the bytes that cross the kernel boundary are
//! `tairix_abi`'s `KeyInput`, the same producer/consumer split as the pointer
//! ([`PointerButton`] vs `tairix_abi`'s `PointerButtonCode`).
//!
//! [`InputRouter`]: https://docs.rs/tairix-wm
//! [`tairix_geometry`]: tairix_geometry

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use tairix_geometry::Point;

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

/// The number of modifiers a keyboard reports, and so how far above the left
/// key's bit the right key's sits: a pair is one contiguous mask, and the
/// eight bits fit the [`ModifierState`] bitmap exactly.
const MODIFIERS: u8 = 4;

/// Which modifier a keyboard's modifier key is, as the index
/// [`ModifierState`] tracks it under.
///
/// The left and right key of a pair are tracked *separately* — releasing one
/// while its sibling is still held must not clear the modifier — and collapse
/// to one [`Modifiers`] flag only when the set is read.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ModifierKey {
    /// A shift key.
    Shift,
    /// A control key.
    Ctrl,
    /// An alt key.
    Alt,
    /// A meta (super / command) key.
    Meta,
}

/// Which of a modifier pair a key is.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ModifierSide {
    /// The left-hand key of the pair.
    Left,
    /// The right-hand key of the pair.
    Right,
}

/// The modifier keys a keyboard currently holds down.
///
/// The one definition of "which modifiers are held", shared by every keyboard
/// producer: each maps its own device's usage or keycode space to a
/// [`ModifierKey`] + [`ModifierSide`] and feeds the edge here, so the
/// left/right collapsing rule and the "did the visible set actually change?"
/// test cannot differ between two drivers.
///
/// [`press`](Self::press) and [`release`](Self::release) report whether the
/// collapsed [`Modifiers`] changed, which is what makes a producer able to
/// emit one edge per *observable* change: holding both shift keys and letting
/// one go is not a change, and a key repeat on a held modifier is not one
/// either.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct ModifierState {
    held: u8,
}

impl ModifierState {
    /// A state with no modifier key held.
    #[must_use]
    pub const fn new() -> Self {
        Self { held: 0 }
    }

    /// Record `key` on `side` going down, reporting whether the collapsed
    /// [`Modifiers`] changed.
    pub fn press(&mut self, key: ModifierKey, side: ModifierSide) -> bool {
        self.set(key, side, true)
    }

    /// Record `key` on `side` coming up, reporting whether the collapsed
    /// [`Modifiers`] changed.
    pub fn release(&mut self, key: ModifierKey, side: ModifierSide) -> bool {
        self.set(key, side, false)
    }

    /// The modifiers held, collapsing each left/right pair.
    #[must_use]
    pub const fn modifiers(self) -> Modifiers {
        Modifiers {
            shift: self.pair_held(ModifierKey::Shift),
            ctrl: self.pair_held(ModifierKey::Ctrl),
            alt: self.pair_held(ModifierKey::Alt),
            meta: self.pair_held(ModifierKey::Meta),
        }
    }

    /// Whether either key of `key`'s pair is held.
    const fn pair_held(self, key: ModifierKey) -> bool {
        let left = 1u8 << Self::index(key, ModifierSide::Left);
        let right = 1u8 << Self::index(key, ModifierSide::Right);
        self.held & (left | right) != 0
    }

    /// Apply one edge, reporting whether the collapsed set changed.
    fn set(&mut self, key: ModifierKey, side: ModifierSide, down: bool) -> bool {
        let before = self.modifiers();
        let bit = 1u8 << Self::index(key, side);
        if down {
            self.held |= bit;
        } else {
            self.held &= !bit;
        }
        self.modifiers() != before
    }

    /// The bit index of one modifier key.
    const fn index(key: ModifierKey, side: ModifierSide) -> u8 {
        let modifier = match key {
            ModifierKey::Shift => 0,
            ModifierKey::Ctrl => 1,
            ModifierKey::Alt => 2,
            ModifierKey::Meta => 3,
        };
        match side {
            ModifierSide::Left => modifier,
            ModifierSide::Right => modifier + MODIFIERS,
        }
    }
}

/// A named non-character key the desktop distinguishes.
///
/// Character-producing keys are not listed here: they arrive as a
/// [`Key::Char`]. This is the closed set of keys that produce no character,
/// matching `tairix_abi`'s wire `NamedKeyCode`.
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
    /// The scroll wheel turned by a relative number of ticks at the current
    /// pointer position. Positive `dx` scrolls toward the logical end,
    /// positive `dy` scrolls downward (the `evdev` orientation). Scroll is a
    /// delta, not an absolute position: the router routes it to the viewport
    /// under the pointer rather than moving the pointer.
    PointerScrolled {
        /// Signed horizontal scroll ticks.
        dx: i32,
        /// Signed vertical scroll ticks.
        dy: i32,
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
    /// The set of held modifiers changed, and no key was produced.
    ///
    /// A modifier key produces no character and is no [`NamedKey`], so it
    /// reaches no surface as a key — but the *state* it leaves behind
    /// qualifies gestures that are not keys at all (a shift-click). The seat
    /// keeps the current set from these edges and stamps it onto what it
    /// routes, so a surface never has to reconstruct it from keys it may
    /// never be sent.
    ModifiersChanged {
        /// The modifiers held after the change.
        modifiers: Modifiers,
    },
}

/// Whether the pointer rests on one surface, as the desktop's seat resolved
/// it: the *enter* and *leave* pair every window system needs.
///
/// This is deliberately **not** an [`InputEvent`]: no device produces it. The
/// seat derives it from the window stack and hands it to the router of each
/// surface whose answer just changed.
///
/// # Why a surface cannot work this out for itself
///
/// A surface knows its own geometry, so it can say whether the pointer is at
/// its coordinates. It cannot say whether anything is drawn *over* it there:
/// the desktop bar's clock stays at the bar's coordinates when a window is
/// dragged across it, and a surface that acted on that position alone would
/// react to gestures the user aimed at the window in front of it — hover
/// feedback lighting up under someone else's window, a hover popover opening
/// over it, a click doing something the user never asked for on a control they
/// could not even see. Stacking is the seat's fact, so the answer comes from
/// the seat.
///
/// # The two rules it carries
///
/// * **A surface acts on pointer input only while it holds the pointer.** The
///   seat delivers pointer events to that one surface's router and to no
///   other, so one press does one thing and it happens where the user was
///   looking.
/// * **A surface is told when it stops holding the pointer.** Hover is state a
///   surface *draws*, so it has to be dropped when the pointer goes — by
///   motion, by a window rising over it, or by a grab taking the pointer
///   elsewhere — or it is left stranded on screen with nothing under it.
///
/// It is a *message*, not state, and deliberately has no [`Default`]: the seat
/// is the one owner of which surface holds the pointer, and a surface that kept
/// its own copy would be a second answer that could disagree with the first.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PointerFocus {
    /// The pointer rests on this surface, at this screen position.
    ///
    /// The position is carried because the pointer can *arrive* without
    /// moving — a window above closed, a grab ended, the surface was raised —
    /// and there is no motion event for the surface to read it from. A
    /// delivered [`InputEvent::PointerMoved`] says the same thing and carries
    /// the same position, so a router that has just been handed one is
    /// already entered.
    Entered {
        /// Where the pointer is, in screen coordinates.
        at: Point,
    },
    /// The pointer rests somewhere else: on another surface, or on nothing.
    Left,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entered_focus_carries_the_position_the_pointer_arrived_at() {
        // The position is the payload: it is what a surface entered without a
        // motion event has to hit-test against.
        let focus = PointerFocus::Entered {
            at: Point::new(4, 9),
        };
        match focus {
            PointerFocus::Entered { at } => assert_eq!(at, Point::new(4, 9)),
            PointerFocus::Left => panic!("expected an enter"),
        }
        assert_ne!(focus, PointerFocus::Left);
    }

    #[test]
    fn a_fresh_modifier_state_holds_nothing() {
        assert_eq!(ModifierState::new().modifiers(), Modifiers::default());
    }

    #[test]
    fn pressing_and_releasing_one_key_reports_both_edges() {
        let mut state = ModifierState::new();
        assert!(state.press(ModifierKey::Shift, ModifierSide::Left));
        assert!(state.modifiers().shift);
        assert!(state.release(ModifierKey::Shift, ModifierSide::Left));
        assert!(!state.modifiers().shift);
    }

    #[test]
    fn a_repeat_of_a_held_key_is_not_a_change() {
        let mut state = ModifierState::new();
        assert!(state.press(ModifierKey::Ctrl, ModifierSide::Right));
        assert!(!state.press(ModifierKey::Ctrl, ModifierSide::Right));
        assert!(state.modifiers().ctrl);
    }

    #[test]
    fn releasing_one_of_a_pair_keeps_the_modifier_held() {
        let mut state = ModifierState::new();
        assert!(state.press(ModifierKey::Shift, ModifierSide::Left));
        // The sibling is already-collapsed state: pressing it changes nothing
        // observable, and releasing it leaves shift held by the other key.
        assert!(!state.press(ModifierKey::Shift, ModifierSide::Right));
        assert!(!state.release(ModifierKey::Shift, ModifierSide::Right));
        assert!(state.modifiers().shift);
        assert!(state.release(ModifierKey::Shift, ModifierSide::Left));
        assert!(!state.modifiers().shift);
    }

    #[test]
    fn releasing_a_key_that_was_never_held_is_not_a_change() {
        let mut state = ModifierState::new();
        assert!(!state.release(ModifierKey::Alt, ModifierSide::Left));
        assert_eq!(state.modifiers(), Modifiers::default());
    }

    #[test]
    fn each_modifier_is_tracked_independently() {
        let mut state = ModifierState::new();
        for key in [
            ModifierKey::Shift,
            ModifierKey::Ctrl,
            ModifierKey::Alt,
            ModifierKey::Meta,
        ] {
            assert!(state.press(key, ModifierSide::Left));
        }
        assert_eq!(
            state.modifiers(),
            Modifiers {
                shift: true,
                ctrl: true,
                alt: true,
                meta: true,
            }
        );
        assert!(state.release(ModifierKey::Alt, ModifierSide::Left));
        assert_eq!(
            state.modifiers(),
            Modifiers {
                shift: true,
                ctrl: true,
                alt: false,
                meta: true,
            }
        );
    }

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
    fn pointer_scrolled_carries_signed_ticks() {
        let event = InputEvent::PointerScrolled { dx: -1, dy: 3 };
        match event {
            InputEvent::PointerScrolled { dx, dy } => {
                assert_eq!((dx, dy), (-1, 3));
            }
            other => panic!("expected a scroll, got {other:?}"),
        }
        assert_ne!(
            InputEvent::PointerScrolled { dx: 0, dy: 1 },
            InputEvent::PointerScrolled { dx: 0, dy: -1 }
        );
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
