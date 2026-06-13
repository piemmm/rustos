//! Console-input producer for a boot-protocol keyboard (`plans/PI.md` P11).
//!
//! [`BootKeyboard`](crate::BootKeyboard) decodes the device's reports into
//! [`InputEvent`] key edges whose `code` is the raw HID usage ID. This module
//! is the second half of the producer: it tracks the held modifiers and the
//! caps-/num-lock state and resolves each printable or named key edge into the
//! [`Key`] a US keyboard layout produces, then emits the *device-resolved key
//! edge* — a [`rustos_abi::input::KeyInput`] record built through the shared
//! [`rustos_keymap::key_input`] map — leaving the encoding and routing to the
//! kernel input-focus arbiter (`AGENTS.md` §17.4, `plans/PI.md` P11). A keyboard
//! driver injects each record into the kernel through the `key_inject` syscall,
//! which decides by who holds input focus whether to encode the press to a text
//! console's tty bytes or deliver the whole record to the desktop.
//!
//! The HID-usage→[`Key`] table is HID-specific (a `ps2` keyboard decodes
//! scancode set 1 into the same [`Key`] vocabulary), so it lives here; the
//! [`Key`]→[`KeyInput`] map is shared in `lib/keymap` (`AGENTS.md` §2.2).
//!
//! Everything here is allocation-free and fail-closed (`AGENTS.md` §2.9): an
//! unknown usage or a non-key event produces no record rather than guessing.

use rustos_abi::driver::input::{Input, InputEvent, InputEventKind};
use rustos_abi::input::KeyInput;
use rustos_abi::DriverError;
use rustos_input::{Key, Modifiers, NamedKey};
use rustos_keymap::key_input;

/// HID usage of the Caps Lock key (HID Usage Tables §10, page `0x07`).
const USAGE_CAPS_LOCK: u16 = 0x39;

/// HID usage of the Num Lock / Clear key.
const USAGE_NUM_LOCK: u16 = 0x53;

/// First modifier usage (`LeftControl`); the eight modifiers occupy
/// `0xE0..=0xE7`, matching [`crate::keyboard::MODIFIER_USAGE_BASE`].
const USAGE_MODIFIER_FIRST: u16 = 0xE0;
/// Last modifier usage (`RightGUI`).
const USAGE_MODIFIER_LAST: u16 = 0xE7;

/// `value` of a press edge in an [`InputEvent`].
const VALUE_PRESS: i32 = 1;
/// `value` of a release edge.
const VALUE_RELEASE: i32 = 0;

/// Sink the [`KeyboardConsole`] producer writes decoded key-edge records to.
///
/// On metal the keyboard driver implements this by calling the `key_inject`
/// syscall with the record's wire bytes; host tests implement it by recording
/// the records (`AGENTS.md` §2.2 — the same seam style as
/// [`crate::ReportSource`]).
pub trait ConsoleSink {
    /// Inject one [`KeyInput`] record's wire bytes
    /// ([`KeyInput::WIRE_LEN`]) into the kernel input-focus arbiter.
    ///
    /// # Errors
    ///
    /// Returns a [`DriverError`] if the underlying delivery failed; the
    /// producer propagates it rather than dropping input.
    fn write(&mut self, bytes: &[u8]) -> Result<(), DriverError>;
}

/// Stateful translation of boot-keyboard [`InputEvent`] edges into console
/// bytes.
///
/// The only state carried between [`feed`](Self::feed) calls is the held
/// modifier bitmap and the two lock toggles, so a reload starts from a clean
/// state by constructing a fresh instance.
#[derive(Debug, Default)]
pub struct KeyboardConsole {
    /// Bit `n` set ⇒ modifier usage `0xE0 + n` is currently held.
    modifier_bits: u8,
    caps_lock: bool,
    num_lock: bool,
}

impl KeyboardConsole {
    /// A producer with no keys held and both locks off.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            modifier_bits: 0,
            caps_lock: false,
            num_lock: false,
        }
    }

    /// The modifiers currently held, collapsing the left/right pairs.
    fn modifiers(&self) -> Modifiers {
        let held = |left: u8, right: u8| self.modifier_bits & ((1u8 << left) | (1u8 << right)) != 0;
        Modifiers {
            ctrl: held(0, 4),
            shift: held(1, 5),
            alt: held(2, 6),
            meta: held(3, 7),
        }
    }

    /// Feed one decoded keyboard [`InputEvent`], returning the
    /// [`KeyInput`] record its key edge resolves to, or [`None`] when the
    /// edge produces no record.
    ///
    /// Modifier edges and the lock keys update the internal state and
    /// produce no record. A *press* or *release* of a printable or named
    /// key produces the corresponding [`KeyInput::Pressed`] /
    /// [`KeyInput::Released`] record carrying the resolved [`Key`] and the
    /// modifiers held; an unknown usage or a non-keyboard event produces
    /// nothing. Both edges are emitted so the desktop sees key-up as well
    /// as key-down; the text path ignores releases in the kernel arbiter.
    ///
    /// # Capabilities
    ///
    /// None (the producer holds no authority; delivery is the sink's).
    pub fn feed(&mut self, event: InputEvent) -> Option<KeyInput> {
        if event.kind != InputEventKind::Key {
            return None;
        }
        let pressed = match event.value {
            VALUE_PRESS => true,
            VALUE_RELEASE => false,
            _ => return None,
        };
        let usage = event.code;
        if (USAGE_MODIFIER_FIRST..=USAGE_MODIFIER_LAST).contains(&usage) {
            let bit = u8::try_from(usage - USAGE_MODIFIER_FIRST).unwrap_or(0);
            if pressed {
                self.modifier_bits |= 1u8 << bit;
            } else {
                self.modifier_bits &= !(1u8 << bit);
            }
            return None;
        }
        if usage == USAGE_CAPS_LOCK {
            if pressed {
                self.caps_lock = !self.caps_lock;
            }
            return None;
        }
        if usage == USAGE_NUM_LOCK {
            if pressed {
                self.num_lock = !self.num_lock;
            }
            return None;
        }
        let modifiers = self.modifiers();
        let key = resolve_usage(usage, modifiers.shift, self.caps_lock, self.num_lock)?;
        // `key_input` returns `None` only for a `Key` with no wire form (a
        // function number outside `F1..=F12`); `resolve_usage` never
        // produces one, so a resolvable key always yields a record.
        key_input(key, modifiers, pressed)
    }
}

/// Poll `keyboard` once and feed every decoded event through `console`,
/// injecting each produced [`KeyInput`] record's wire bytes into `sink`.
///
/// This is the keyboard driver's per-iteration loop: on metal the driver calls
/// it in its service loop with a [`ConsoleSink`] that invokes `key_inject`;
/// host tests call it with a recording sink. Returns the number of events
/// drained from the keyboard this call.
///
/// # Errors
///
/// Propagates a [`DriverError`] from the keyboard `poll` or from the `sink` —
/// input is never silently dropped.
///
/// # Capabilities
///
/// None beyond those the keyboard and sink already hold.
pub fn pump_once<I: Input, S: ConsoleSink>(
    keyboard: &mut I,
    console: &mut KeyboardConsole,
    sink: &mut S,
) -> Result<usize, DriverError> {
    let mut events = [crate::EVENT_ZERO; EVENT_BATCH];
    let drained = keyboard.poll(&mut events)?;
    for event in &events[..drained] {
        if let Some(record) = console.feed(*event) {
            sink.write(&record.to_le_bytes())?;
        }
    }
    Ok(drained)
}

/// Events drained from the keyboard per [`pump_once`] call.
///
/// A batch size, not a capacity (`AGENTS.md` §24.4): undrained reports stay
/// queued in the keyboard's [`crate::ReportSource`] and are read next call.
pub const EVENT_BATCH: usize = 16;

/// Resolve a HID page-`0x07` usage to the [`Key`] a US keyboard layout
/// produces, given the active `shift`, `caps`, and `num` lock state.
///
/// Returns `None` for a usage with no console key (an unmapped usage, a lock
/// key, or numeric-keypad `5` with Num Lock off) — fail closed, never guess
/// (`AGENTS.md` §2.9).
fn resolve_usage(usage: u16, shift: bool, caps: bool, num: bool) -> Option<Key> {
    if let Some(letter) = letter(usage, shift, caps) {
        return Some(Key::Char(letter));
    }
    if let Some(ch) = printable(usage, shift) {
        return Some(Key::Char(ch));
    }
    if let Some(named) = named(usage) {
        return Some(Key::Named(named));
    }
    keypad(usage, num)
}

/// Map a letter usage (`0x04`=`A`..`0x1D`=`Z`) to its character, applying
/// shift XOR caps lock for the case.
fn letter(usage: u16, shift: bool, caps: bool) -> Option<char> {
    if (0x04..=0x1D).contains(&usage) {
        let offset = u8::try_from(usage - 0x04).ok()?;
        let lower = b'a' + offset;
        let byte = if shift ^ caps {
            lower.to_ascii_uppercase()
        } else {
            lower
        };
        return Some(char::from(byte));
    }
    None
}

/// Map a printable non-letter usage (digits, space, punctuation) to its
/// character, selecting the shifted glyph when `shift` is held.
fn printable(usage: u16, shift: bool) -> Option<char> {
    // (usage, unshifted, shifted). The US ANSI layout's non-letter printables.
    const TABLE: &[(u16, char, char)] = &[
        (0x1E, '1', '!'),
        (0x1F, '2', '@'),
        (0x20, '3', '#'),
        (0x21, '4', '$'),
        (0x22, '5', '%'),
        (0x23, '6', '^'),
        (0x24, '7', '&'),
        (0x25, '8', '*'),
        (0x26, '9', '('),
        (0x27, '0', ')'),
        (0x2C, ' ', ' '),
        (0x2D, '-', '_'),
        (0x2E, '=', '+'),
        (0x2F, '[', '{'),
        (0x30, ']', '}'),
        (0x31, '\\', '|'),
        (0x33, ';', ':'),
        (0x34, '\'', '"'),
        (0x35, '`', '~'),
        (0x36, ',', '<'),
        (0x37, '.', '>'),
        (0x38, '/', '?'),
    ];
    for &(code, unshifted, shifted) in TABLE {
        if code == usage {
            return Some(if shift { shifted } else { unshifted });
        }
    }
    None
}

/// Map a named-key usage (editing, navigation, function, the main-block
/// control keys) to its [`NamedKey`].
fn named(usage: u16) -> Option<NamedKey> {
    Some(match usage {
        0x28 => NamedKey::Enter,
        0x29 => NamedKey::Escape,
        0x2A => NamedKey::Backspace,
        0x2B => NamedKey::Tab,
        0x49 => NamedKey::Insert,
        0x4A => NamedKey::Home,
        0x4B => NamedKey::PageUp,
        0x4C => NamedKey::Delete,
        0x4D => NamedKey::End,
        0x4E => NamedKey::PageDown,
        0x4F => NamedKey::Right,
        0x50 => NamedKey::Left,
        0x51 => NamedKey::Down,
        0x52 => NamedKey::Up,
        0x3A..=0x45 => NamedKey::Function {
            number: u8::try_from(usage - 0x3A + 1).ok()?,
        },
        _ => return None,
    })
}

/// Map a numeric-keypad usage to its [`Key`], honouring Num Lock.
///
/// With Num Lock on the keypad sends digits, `.`, and the operators; with it
/// off the digit keys are the navigation cluster (`KP1`=End, `KP8`=Up, …) and
/// `KP5` sends nothing.
fn keypad(usage: u16, num: bool) -> Option<Key> {
    Some(match usage {
        0x54 => Key::Char('/'),
        0x55 => Key::Char('*'),
        0x56 => Key::Char('-'),
        0x57 => Key::Char('+'),
        0x58 => Key::Named(NamedKey::Enter),
        0x59 => keypad_digit('1', NamedKey::End, num),
        0x5A => keypad_digit('2', NamedKey::Down, num),
        0x5B => keypad_digit('3', NamedKey::PageDown, num),
        0x5C => keypad_digit('4', NamedKey::Left, num),
        0x5D => {
            if num {
                Key::Char('5')
            } else {
                return None;
            }
        }
        0x5E => keypad_digit('6', NamedKey::Right, num),
        0x5F => keypad_digit('7', NamedKey::Home, num),
        0x60 => keypad_digit('8', NamedKey::Up, num),
        0x61 => keypad_digit('9', NamedKey::PageUp, num),
        0x62 => keypad_digit('0', NamedKey::Insert, num),
        0x63 => {
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
