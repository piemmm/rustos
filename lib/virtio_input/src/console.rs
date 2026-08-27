//! Console-input producer for a virtio-input keyboard (`plans/PI.md` P11 /
//! P10 chunk 5d-2-ii).
//!
//! [`VirtioInput`](crate::VirtioInput) decodes the device's eventq into
//! [`InputEvent`] key edges whose `code` is the raw Linux `evdev` keycode
//! (virtio-input reports in the `evdev` namespace, virtio 1.1 §5.8). This
//! module is the producer half: it tracks the held modifiers and the
//! caps-/num-lock state and resolves each printable or named key edge into
//! the [`Key`] a US keyboard layout produces, then emits the *device-resolved
//! key edge* — a [`tairix_abi::input::KeyInput`] record built through the
//! shared [`tairix_keymap::key_input`] map — leaving the encoding and routing
//! to the kernel input-focus arbiter. A keyboard driver
//! injects each record into the kernel through the `key_inject` syscall, which
//! decides by who holds focus whether to encode the press to a text console's
//! tty bytes or deliver the whole record to the desktop.
//!
//! The `evdev`-keycode→[`Key`] table is `evdev`-specific (a USB HID boot
//! keyboard decodes HID usages into the same [`Key`] vocabulary in
//! `lib/hid`), so it lives here; the [`Key`]→[`KeyInput`] map is shared in
//! `lib/keymap` (one definition, the `lib/hid` console
//! producer reaches it too).
//!
//! Everything here is allocation-free and fail-closed: an
//! unknown keycode or a non-key event produces no record rather than guessing.

use tairix_abi::driver::input::{InputEvent, InputEventKind};
use tairix_abi::input::KeyInput;
use tairix_input::{Key, ModifierKey, ModifierSide, ModifierState, NamedKey};
use tairix_keymap::{key_input, modifier_change};

/// Linux `evdev` keycodes this producer recognises (`input-event-codes.h`).
/// Only the constants the resolver and the modifier/lock tracking name are
/// listed; the resolution tables below carry the rest inline.
mod code {
    /// `KEY_ESC`.
    pub const ESC: u16 = 1;
    /// `KEY_BACKSPACE`.
    pub const BACKSPACE: u16 = 14;
    /// `KEY_TAB`.
    pub const TAB: u16 = 15;
    /// `KEY_ENTER` (main block).
    pub const ENTER: u16 = 28;
    /// `KEY_KPENTER` (numeric keypad).
    pub const KP_ENTER: u16 = 96;

    /// `KEY_CAPSLOCK`.
    pub const CAPS_LOCK: u16 = 58;
    /// `KEY_NUMLOCK`.
    pub const NUM_LOCK: u16 = 69;

    /// `KEY_LEFTCTRL`.
    pub const LEFT_CTRL: u16 = 29;
    /// `KEY_LEFTSHIFT`.
    pub const LEFT_SHIFT: u16 = 42;
    /// `KEY_RIGHTSHIFT`.
    pub const RIGHT_SHIFT: u16 = 54;
    /// `KEY_LEFTALT`.
    pub const LEFT_ALT: u16 = 56;
    /// `KEY_RIGHTCTRL`.
    pub const RIGHT_CTRL: u16 = 97;
    /// `KEY_RIGHTALT`.
    pub const RIGHT_ALT: u16 = 100;
    /// `KEY_LEFTMETA`.
    pub const LEFT_META: u16 = 125;
    /// `KEY_RIGHTMETA`.
    pub const RIGHT_META: u16 = 126;

    /// `KEY_HOME`.
    pub const HOME: u16 = 102;
    /// `KEY_UP`.
    pub const UP: u16 = 103;
    /// `KEY_PAGEUP`.
    pub const PAGE_UP: u16 = 104;
    /// `KEY_LEFT`.
    pub const LEFT: u16 = 105;
    /// `KEY_RIGHT`.
    pub const RIGHT: u16 = 106;
    /// `KEY_END`.
    pub const END: u16 = 107;
    /// `KEY_DOWN`.
    pub const DOWN: u16 = 108;
    /// `KEY_PAGEDOWN`.
    pub const PAGE_DOWN: u16 = 109;
    /// `KEY_INSERT`.
    pub const INSERT: u16 = 110;
    /// `KEY_DELETE`.
    pub const DELETE: u16 = 111;

    /// First main-block function key (`KEY_F1`); `F1..=F10` are `59..=68`.
    pub const F1: u16 = 59;
    /// Last contiguous main-block function key (`KEY_F10`).
    pub const F10: u16 = 68;
    /// `KEY_F11` (out of the `F1..=F10` run).
    pub const F11: u16 = 87;
    /// `KEY_F12`.
    pub const F12: u16 = 88;
}

/// `value` of a press edge in an [`InputEvent`].
const VALUE_PRESS: i32 = 1;
/// `value` of a release edge.
const VALUE_RELEASE: i32 = 0;

/// Stateful translation of virtio-input keyboard [`InputEvent`] edges into the
/// [`KeyInput`] records a driver injects through `key_inject`.
///
/// The only state carried between [`feed`](Self::feed) calls is the held
/// modifiers and the two lock toggles, so a reload starts from a clean
/// state by constructing a fresh instance.
#[derive(Debug, Default)]
pub struct VirtioKeyboardConsole {
    modifiers: ModifierState,
    caps_lock: bool,
    num_lock: bool,
}

impl VirtioKeyboardConsole {
    /// A producer with no keys held and both locks off.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            modifiers: ModifierState::new(),
            caps_lock: false,
            num_lock: false,
        }
    }

    /// Feed one decoded keyboard [`InputEvent`], returning the [`KeyInput`]
    /// record its key edge resolves to, or [`None`] when the edge produces no
    /// record.
    ///
    /// A modifier edge that changes the *observable* set of held modifiers
    /// produces a [`KeyInput::ModifiersChanged`] record — the desktop needs it
    /// to qualify a gesture that is not a key (a shift-click) — while one that
    /// does not (a repeat, or letting go of one shift key while the other is
    /// held) produces nothing. The lock keys update the internal state and
    /// produce no record. A *press* or *release* of a printable or named key
    /// produces the corresponding [`KeyInput::Pressed`] / [`KeyInput::Released`]
    /// record carrying the resolved [`Key`] and the modifiers held; an unknown
    /// keycode or a non-keyboard event produces nothing. Both edges are
    /// emitted so the desktop sees key-up as well as key-down; the text path
    /// ignores releases in the kernel arbiter.
    ///
    /// # Capabilities
    ///
    /// None (the producer holds no authority; delivery is the caller's).
    pub fn feed(&mut self, event: InputEvent) -> Option<KeyInput> {
        if event.kind != InputEventKind::Key {
            return None;
        }
        let pressed = match event.value {
            VALUE_PRESS => true,
            VALUE_RELEASE => false,
            // A key-repeat (`value == 2`) or any other value carries no
            // press/release edge here (fail closed).
            _ => return None,
        };
        let keycode = event.code;
        if let Some((key, side)) = modifier_of(keycode) {
            let changed = if pressed {
                self.modifiers.press(key, side)
            } else {
                self.modifiers.release(key, side)
            };
            return changed.then(|| modifier_change(self.modifiers.modifiers()));
        }
        if keycode == code::CAPS_LOCK {
            if pressed {
                self.caps_lock = !self.caps_lock;
            }
            return None;
        }
        if keycode == code::NUM_LOCK {
            if pressed {
                self.num_lock = !self.num_lock;
            }
            return None;
        }
        let modifiers = self.modifiers.modifiers();
        let key = resolve_keycode(keycode, modifiers.shift, self.caps_lock, self.num_lock)?;
        // `key_input` returns `None` only for a `Key` with no wire form (a
        // function number outside `F1..=F12`); `resolve_keycode` never
        // produces one, so a resolvable key always yields a record.
        key_input(key, modifiers, pressed)
    }
}

/// The modifier a modifier keycode names, or [`None`] if `keycode` is not a
/// modifier key.
fn modifier_of(keycode: u16) -> Option<(ModifierKey, ModifierSide)> {
    Some(match keycode {
        code::LEFT_CTRL => (ModifierKey::Ctrl, ModifierSide::Left),
        code::LEFT_SHIFT => (ModifierKey::Shift, ModifierSide::Left),
        code::LEFT_ALT => (ModifierKey::Alt, ModifierSide::Left),
        code::LEFT_META => (ModifierKey::Meta, ModifierSide::Left),
        code::RIGHT_CTRL => (ModifierKey::Ctrl, ModifierSide::Right),
        code::RIGHT_SHIFT => (ModifierKey::Shift, ModifierSide::Right),
        code::RIGHT_ALT => (ModifierKey::Alt, ModifierSide::Right),
        code::RIGHT_META => (ModifierKey::Meta, ModifierSide::Right),
        _ => return None,
    })
}

/// Resolve an `evdev` keycode to the [`Key`] a US keyboard layout produces,
/// given the active `shift`, `caps`, and `num` lock state.
///
/// Returns `None` for a keycode with no console key (an unmapped keycode, a
/// lock or modifier key, or numeric-keypad `5` with Num Lock off) — fail
/// closed, never guess.
fn resolve_keycode(keycode: u16, shift: bool, caps: bool, num: bool) -> Option<Key> {
    if let Some(letter) = letter(keycode, shift, caps) {
        return Some(Key::Char(letter));
    }
    if let Some(ch) = printable(keycode, shift) {
        return Some(Key::Char(ch));
    }
    if let Some(named) = named(keycode) {
        return Some(Key::Named(named));
    }
    keypad(keycode, num)
}

/// Map a letter keycode to its character, applying shift XOR caps lock for
/// the case. `evdev` letter keycodes are not alphabetical, so the lowercase
/// glyph is looked up in a table rather than computed from an offset.
fn letter(keycode: u16, shift: bool, caps: bool) -> Option<char> {
    // (keycode, lowercase letter). The US QWERTY letter block.
    const TABLE: &[(u16, u8)] = &[
        (16, b'q'),
        (17, b'w'),
        (18, b'e'),
        (19, b'r'),
        (20, b't'),
        (21, b'y'),
        (22, b'u'),
        (23, b'i'),
        (24, b'o'),
        (25, b'p'),
        (30, b'a'),
        (31, b's'),
        (32, b'd'),
        (33, b'f'),
        (34, b'g'),
        (35, b'h'),
        (36, b'j'),
        (37, b'k'),
        (38, b'l'),
        (44, b'z'),
        (45, b'x'),
        (46, b'c'),
        (47, b'v'),
        (48, b'b'),
        (49, b'n'),
        (50, b'm'),
    ];
    for &(code, lower) in TABLE {
        if code == keycode {
            let byte = if shift ^ caps {
                lower.to_ascii_uppercase()
            } else {
                lower
            };
            return Some(char::from(byte));
        }
    }
    None
}

/// Map a printable non-letter keycode (digits, space, punctuation) to its
/// character, selecting the shifted glyph when `shift` is held.
fn printable(keycode: u16, shift: bool) -> Option<char> {
    // (keycode, unshifted, shifted). The US ANSI layout's non-letter
    // printables.
    const TABLE: &[(u16, char, char)] = &[
        (2, '1', '!'),
        (3, '2', '@'),
        (4, '3', '#'),
        (5, '4', '$'),
        (6, '5', '%'),
        (7, '6', '^'),
        (8, '7', '&'),
        (9, '8', '*'),
        (10, '9', '('),
        (11, '0', ')'),
        (12, '-', '_'),
        (13, '=', '+'),
        (26, '[', '{'),
        (27, ']', '}'),
        (39, ';', ':'),
        (40, '\'', '"'),
        (41, '`', '~'),
        (43, '\\', '|'),
        (51, ',', '<'),
        (52, '.', '>'),
        (53, '/', '?'),
        (57, ' ', ' '),
    ];
    for &(code, unshifted, shifted) in TABLE {
        if code == keycode {
            return Some(if shift { shifted } else { unshifted });
        }
    }
    None
}

/// Map a named-key keycode (editing, navigation, function, the main-block
/// control keys) to its [`NamedKey`].
fn named(keycode: u16) -> Option<NamedKey> {
    Some(match keycode {
        code::ENTER | code::KP_ENTER => NamedKey::Enter,
        code::ESC => NamedKey::Escape,
        code::BACKSPACE => NamedKey::Backspace,
        code::TAB => NamedKey::Tab,
        code::INSERT => NamedKey::Insert,
        code::HOME => NamedKey::Home,
        code::PAGE_UP => NamedKey::PageUp,
        code::DELETE => NamedKey::Delete,
        code::END => NamedKey::End,
        code::PAGE_DOWN => NamedKey::PageDown,
        code::RIGHT => NamedKey::Right,
        code::LEFT => NamedKey::Left,
        code::DOWN => NamedKey::Down,
        code::UP => NamedKey::Up,
        code::F1..=code::F10 => NamedKey::Function {
            number: u8::try_from(keycode - code::F1 + 1).ok()?,
        },
        code::F11 => NamedKey::Function { number: 11 },
        code::F12 => NamedKey::Function { number: 12 },
        _ => return None,
    })
}

/// Map a numeric-keypad keycode to its [`Key`], honouring Num Lock.
///
/// With Num Lock on the keypad sends digits, `.`, and the operators; with it
/// off the digit keys are the navigation cluster (`KP1`=End, `KP8`=Up, …) and
/// `KP5` sends nothing.
fn keypad(keycode: u16, num: bool) -> Option<Key> {
    Some(match keycode {
        98 => Key::Char('/'), // KEY_KPSLASH
        55 => Key::Char('*'), // KEY_KPASTERISK
        74 => Key::Char('-'), // KEY_KPMINUS
        78 => Key::Char('+'), // KEY_KPPLUS
        71 => keypad_digit('7', NamedKey::Home, num),
        72 => keypad_digit('8', NamedKey::Up, num),
        73 => keypad_digit('9', NamedKey::PageUp, num),
        75 => keypad_digit('4', NamedKey::Left, num),
        76 => {
            if num {
                Key::Char('5')
            } else {
                return None;
            }
        }
        77 => keypad_digit('6', NamedKey::Right, num),
        79 => keypad_digit('1', NamedKey::End, num),
        80 => keypad_digit('2', NamedKey::Down, num),
        81 => keypad_digit('3', NamedKey::PageDown, num),
        82 => keypad_digit('0', NamedKey::Insert, num),
        83 => {
            if num {
                Key::Char('.')
            } else {
                Key::Named(NamedKey::Delete)
            }
        }
        _ => return None,
    })
}

/// A keypad key that is `digit` with Num Lock on and `nav` with it off.
fn keypad_digit(digit: char, nav: NamedKey, num: bool) -> Key {
    if num {
        Key::Char(digit)
    } else {
        Key::Named(nav)
    }
}

#[cfg(test)]
mod tests;
