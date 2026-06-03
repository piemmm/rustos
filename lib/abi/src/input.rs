//! Wire-level pointer input event delivered over the kernel input channel.
//!
//! A pointing device reports motion, presses, and releases. On a running
//! system those reports reach the user-space desktop as a stream of framed
//! records over a capability-checked kernel input channel; this module fixes
//! the *contract* of one such record, [`PointerInput`]. The desktop's
//! `no_std` GUI vocabulary (`lib/input`'s `InputEvent`) is the in-process
//! shape the window manager and taskbar route; this ABI record is the bytes
//! that travel the kernel boundary and decode into it (`AGENTS.md` §9 / §10 /
//! §17.4). The kernel never hands a parser the device's structure: it hands a
//! buffer, and [`PointerInput::from_bytes`] validates every field and fails
//! closed (§5.4 / §19.5).
//!
//! Keyboard input is modelled alongside the pointer by [`KeyInput`]: the
//! *desktop-level* key event the window manager delivers to the focused
//! surface — a produced [`KeyValue`] (a Unicode character, or a named
//! non-character key) plus the [`Modifiers`] held while it was produced. As
//! with the pointer, this is **not** the device-level report: turning raw
//! platform keycodes and a keyboard layout into a produced character is policy
//! that lives above the driver, not a second copy of the same data (§2.2).
//! [`KeyInput::from_bytes`] validates every field and fails closed on
//! untrusted input (§5.4 / §19.5).
//!
//! # Relationship to the driver input ABI
//!
//! This is **not** a duplicate of [`crate::driver::input::InputEvent`]
//! (`AGENTS.md` §2.2). That type is the *device-level* contract an input
//! driver reports across the [`Input`](crate::driver::input::Input) driver
//! trait: raw per-axis pointer *deltas*, scroll ticks, and platform keycodes.
//! [`PointerInput`] is the *desktop-level* event the window manager and
//! taskbar route: an **absolute** pointer position and a *resolved* pointer
//! button (primary / secondary / middle). Turning the device-relative driver
//! stream into this resolved stream — accumulating deltas into a position,
//! deciding which keycodes are pointer buttons — is pointer-input policy that
//! lives above the driver, not a second copy of the same data. The two ABIs
//! sit on opposite sides of that policy.
//!
//! # Wire layout
//!
//! A record is exactly [`PointerInput::WIRE_LEN`] bytes, little-endian:
//!
//! | offset | size | field      | meaning                                  |
//! |-------:|-----:|------------|------------------------------------------|
//! |      0 |    4 | `magic`    | [`POINTER_INPUT_MAGIC`]                   |
//! |      4 |    2 | `version`  | ABI version ([`crate::ABI_VERSION_CURRENT`]) |
//! |      6 |    2 | `kind`     | [`KIND_MOVED`] / [`KIND_PRESSED`] / [`KIND_RELEASED`] |
//! |      8 |    2 | `button`   | [`BUTTON_NONE`] for motion, else a button code |
//! |     10 |    2 | `reserved` | must be zero                             |
//! |     12 |    4 | `x`        | absolute screen x (motion only)          |
//! |     16 |    4 | `y`        | absolute screen y (motion only)          |
//!
//! The coordinate fields carry the new pointer position for a
//! [`Moved`](PointerInput::Moved) record and must be zero for a press or
//! release (a real pointing device reports motion separately from clicks, and
//! a router applies a button at the position the last motion established —
//! the same model as `lib/input`). The `button` field is [`BUTTON_NONE`] for
//! motion and a valid button code for a press or release. Any other
//! combination is wire corruption and is refused.

use crate::le::{put_i32, put_u16, put_u32, read_i32, read_u16, read_u32};
use crate::Errno;

/// Magic number identifying an `abi-v1` pointer input record (`"PIN1"`
/// little-endian).
pub const POINTER_INPUT_MAGIC: u32 = u32::from_le_bytes(*b"PIN1");

/// `kind` code for an absolute pointer move.
pub const KIND_MOVED: u16 = 0;
/// `kind` code for a pointer-button press.
pub const KIND_PRESSED: u16 = 1;
/// `kind` code for a pointer-button release.
pub const KIND_RELEASED: u16 = 2;

/// `button` code carried by a [`Moved`](PointerInput::Moved) record: no
/// button is involved in a motion event.
pub const BUTTON_NONE: u16 = 0;

/// The pointer button a press or release record names.
///
/// A `#[repr(u16)]` enum so the wire code is exactly the integer carried in
/// the record's `button` field. The discriminants are part of the frozen
/// pointer-input ABI; [`PointerButtonCode::from_code`] rejects any other
/// value rather than guessing (`AGENTS.md` §5.4 — validate every input).
#[repr(u16)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum PointerButtonCode {
    /// The primary (typically left) button: activates and grabs.
    Primary = 1,
    /// The secondary (typically right) button: context actions.
    Secondary = 2,
    /// The middle button.
    Middle = 3,
}

impl PointerButtonCode {
    /// The wire code carried in a record's `button` field.
    #[must_use]
    pub const fn code(self) -> u16 {
        self as u16
    }

    /// Decode a `button` field into a [`PointerButtonCode`].
    ///
    /// # Errors
    ///
    /// Returns [`Errno::OutOfRange`] for any value other than the three
    /// defined button codes (including [`BUTTON_NONE`], which is valid only
    /// on a motion record).
    pub const fn from_code(code: u16) -> Result<Self, Errno> {
        match code {
            1 => Ok(Self::Primary),
            2 => Ok(Self::Secondary),
            3 => Ok(Self::Middle),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// A single decoded pointer input event.
///
/// The type makes illegal states unrepresentable (`AGENTS.md` §2.11): a
/// [`Moved`](Self::Moved) carries a position and no button, while a
/// [`Pressed`](Self::Pressed) / [`Released`](Self::Released) carries a button
/// and no position. [`from_bytes`](Self::from_bytes) is the only way to build
/// one from untrusted bytes and validates the whole record before returning
/// it.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum PointerInput {
    /// The pointer moved to an absolute screen position.
    Moved {
        /// New pointer x coordinate, in screen pixels.
        x: i32,
        /// New pointer y coordinate, in screen pixels.
        y: i32,
    },
    /// A pointer button went down at the current pointer position.
    Pressed(PointerButtonCode),
    /// A pointer button came up at the current pointer position.
    Released(PointerButtonCode),
}

impl PointerInput {
    /// Encoded size of a pointer input record on the wire, in bytes.
    pub const WIRE_LEN: usize = 20;

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, POINTER_INPUT_MAGIC);
        put_u16(&mut out, 4, crate::ABI_VERSION_CURRENT_U16);
        let (kind, button, x, y) = match *self {
            Self::Moved { x, y } => (KIND_MOVED, BUTTON_NONE, x, y),
            Self::Pressed(button) => (KIND_PRESSED, button.code(), 0, 0),
            Self::Released(button) => (KIND_RELEASED, button.code(), 0, 0),
        };
        put_u16(&mut out, 6, kind);
        put_u16(&mut out, 8, button);
        // bytes 10..12 reserved, already zero.
        put_i32(&mut out, 12, x);
        put_i32(&mut out, 16, y);
        out
    }

    /// Decode `bytes` into a [`PointerInput`].
    ///
    /// Every field is validated; an invalid record is refused rather than
    /// interpreted (`AGENTS.md` §5.4 / §19.5 — fail closed on untrusted
    /// input).
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::BadMagic`] if the magic word does not match, or if the
    ///   reserved field is non-zero, or if a press/release record carries a
    ///   non-zero coordinate (a coordinate belongs to a motion record only).
    /// * [`Errno::AbiVersionUnsupported`] if `version` is not
    ///   [`crate::ABI_VERSION_CURRENT`].
    /// * [`Errno::OutOfRange`] if `kind` is not a defined kind, or if the
    ///   `button` field is inconsistent with the kind (a button code on a
    ///   motion record, or [`BUTTON_NONE`]/an unknown code on a press or
    ///   release).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != POINTER_INPUT_MAGIC {
            return Err(Errno::BadMagic);
        }
        if u32::from(read_u16(bytes, 4)) != crate::ABI_VERSION_CURRENT {
            return Err(Errno::AbiVersionUnsupported);
        }
        if read_u16(bytes, 10) != 0 {
            return Err(Errno::BadMagic);
        }
        let kind = read_u16(bytes, 6);
        let button = read_u16(bytes, 8);
        let x = read_i32(bytes, 12);
        let y = read_i32(bytes, 16);
        match kind {
            KIND_MOVED => {
                if button != BUTTON_NONE {
                    return Err(Errno::OutOfRange);
                }
                Ok(Self::Moved { x, y })
            }
            KIND_PRESSED | KIND_RELEASED => {
                if x != 0 || y != 0 {
                    return Err(Errno::BadMagic);
                }
                let button = PointerButtonCode::from_code(button)?;
                Ok(if kind == KIND_PRESSED {
                    Self::Pressed(button)
                } else {
                    Self::Released(button)
                })
            }
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Magic number identifying an `abi-v1` keyboard input record (`"KIN1"`
/// little-endian).
pub const KEY_INPUT_MAGIC: u32 = u32::from_le_bytes(*b"KIN1");

/// `kind` code for a key-press record.
pub const KIND_KEY_PRESSED: u16 = 1;
/// `kind` code for a key-release record.
pub const KIND_KEY_RELEASED: u16 = 2;

/// `key_class` code for a [`KeyValue::Char`] record (a produced character).
pub const KEY_CLASS_CHAR: u16 = 0;
/// `key_class` code for a [`KeyValue::Named`] record (a named non-character
/// key).
pub const KEY_CLASS_NAMED: u16 = 1;

/// Modifier bit set when the shift key was held.
pub const MOD_SHIFT: u16 = 0x1;
/// Modifier bit set when the control key was held.
pub const MOD_CTRL: u16 = 0x2;
/// Modifier bit set when the alt key was held.
pub const MOD_ALT: u16 = 0x4;
/// Modifier bit set when the meta (super / command) key was held.
pub const MOD_META: u16 = 0x8;
/// Mask of every defined modifier bit; any bit outside it is wire corruption.
pub const MOD_MASK: u16 = MOD_SHIFT | MOD_CTRL | MOD_ALT | MOD_META;

/// The keyboard modifiers held while a key event was produced.
///
/// A plain set of booleans rather than a bitmask type so a caller names the
/// modifier it cares about; [`from_bits`](Self::from_bits) rejects any
/// undefined bit rather than silently dropping it (`AGENTS.md` §5.4).
//
// The four booleans are independent modifier-key states, not a state
// machine: any combination is legal (Ctrl+Shift, Alt+Meta, none), so a flat
// record models them more clearly than an enum.
#[allow(clippy::struct_excessive_bools)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
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

impl Modifiers {
    /// Encode the modifiers as the record's little-endian bitmask field.
    #[must_use]
    pub const fn to_bits(self) -> u16 {
        let mut bits = 0;
        if self.shift {
            bits |= MOD_SHIFT;
        }
        if self.ctrl {
            bits |= MOD_CTRL;
        }
        if self.alt {
            bits |= MOD_ALT;
        }
        if self.meta {
            bits |= MOD_META;
        }
        bits
    }

    /// Decode a modifier bitmask, rejecting any bit outside [`MOD_MASK`].
    ///
    /// # Errors
    ///
    /// Returns [`Errno::OutOfRange`] if `bits` sets a bit this ABI version does
    /// not define (`AGENTS.md` §5.4 — validate every input, fail closed).
    pub const fn from_bits(bits: u16) -> Result<Self, Errno> {
        if bits & !MOD_MASK != 0 {
            return Err(Errno::OutOfRange);
        }
        Ok(Self {
            shift: bits & MOD_SHIFT != 0,
            ctrl: bits & MOD_CTRL != 0,
            alt: bits & MOD_ALT != 0,
            meta: bits & MOD_META != 0,
        })
    }
}

/// A named non-character key the desktop distinguishes.
///
/// A `#[repr(u16)]` enum so the wire code is exactly the integer carried in a
/// record's `named` field. The discriminants are part of the keyboard ABI;
/// [`NamedKeyCode::from_code`] rejects any other value rather than guessing
/// (`AGENTS.md` §5.4). Character-producing keys (letters, digits, punctuation,
/// space) are *not* listed here — they travel as a [`KeyValue::Char`], so this
/// set stays the small, closed list of keys that produce no character.
#[repr(u16)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum NamedKeyCode {
    /// The Enter / Return key.
    Enter = 1,
    /// The Escape key.
    Escape = 2,
    /// The Backspace key.
    Backspace = 3,
    /// The Tab key.
    Tab = 4,
    /// The forward-delete key.
    Delete = 5,
    /// The Insert key.
    Insert = 6,
    /// The Home key.
    Home = 7,
    /// The End key.
    End = 8,
    /// The Page Up key.
    PageUp = 9,
    /// The Page Down key.
    PageDown = 10,
    /// The left-arrow key.
    Left = 11,
    /// The right-arrow key.
    Right = 12,
    /// The up-arrow key.
    Up = 13,
    /// The down-arrow key.
    Down = 14,
    /// Function key F1.
    F1 = 15,
    /// Function key F2.
    F2 = 16,
    /// Function key F3.
    F3 = 17,
    /// Function key F4.
    F4 = 18,
    /// Function key F5.
    F5 = 19,
    /// Function key F6.
    F6 = 20,
    /// Function key F7.
    F7 = 21,
    /// Function key F8.
    F8 = 22,
    /// Function key F9.
    F9 = 23,
    /// Function key F10.
    F10 = 24,
    /// Function key F11.
    F11 = 25,
    /// Function key F12.
    F12 = 26,
}

impl NamedKeyCode {
    /// The wire code carried in a record's `named` field.
    #[must_use]
    pub const fn code(self) -> u16 {
        self as u16
    }

    /// Decode a `named` field into a [`NamedKeyCode`].
    ///
    /// # Errors
    ///
    /// Returns [`Errno::OutOfRange`] for any value that is not a defined named
    /// key (including `0`, which marks "no named key" on a character record).
    pub const fn from_code(code: u16) -> Result<Self, Errno> {
        match code {
            1 => Ok(Self::Enter),
            2 => Ok(Self::Escape),
            3 => Ok(Self::Backspace),
            4 => Ok(Self::Tab),
            5 => Ok(Self::Delete),
            6 => Ok(Self::Insert),
            7 => Ok(Self::Home),
            8 => Ok(Self::End),
            9 => Ok(Self::PageUp),
            10 => Ok(Self::PageDown),
            11 => Ok(Self::Left),
            12 => Ok(Self::Right),
            13 => Ok(Self::Up),
            14 => Ok(Self::Down),
            15 => Ok(Self::F1),
            16 => Ok(Self::F2),
            17 => Ok(Self::F3),
            18 => Ok(Self::F4),
            19 => Ok(Self::F5),
            20 => Ok(Self::F6),
            21 => Ok(Self::F7),
            22 => Ok(Self::F8),
            23 => Ok(Self::F9),
            24 => Ok(Self::F10),
            25 => Ok(Self::F11),
            26 => Ok(Self::F12),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// The key a [`KeyInput`] names: either a produced character or a named
/// non-character key.
///
/// The type makes illegal states unrepresentable (`AGENTS.md` §2.11): a
/// character key carries exactly its Unicode scalar, a named key exactly its
/// code, and the two never coexist in one record.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum KeyValue {
    /// A character-producing key, carrying the produced Unicode scalar (after
    /// the keyboard layout and any modifiers have been applied).
    Char(char),
    /// A named non-character key (an editing, navigation, or function key).
    Named(NamedKeyCode),
}

/// A single decoded keyboard input event.
///
/// The desktop-level counterpart of [`PointerInput`]: a key going down or
/// coming up, the [`KeyValue`] it produced, and the [`Modifiers`] held at the
/// time. [`from_bytes`](Self::from_bytes) is the only way to build one from
/// untrusted bytes and validates the whole record before returning it.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum KeyInput {
    /// A key went down.
    Pressed {
        /// The key that was pressed.
        key: KeyValue,
        /// The modifiers held while it was pressed.
        modifiers: Modifiers,
    },
    /// A key came up.
    Released {
        /// The key that was released.
        key: KeyValue,
        /// The modifiers held while it was released.
        modifiers: Modifiers,
    },
}

impl KeyInput {
    /// Encoded size of a keyboard input record on the wire, in bytes.
    pub const WIRE_LEN: usize = 20;

    /// The [`KeyValue`] this event carries.
    #[must_use]
    pub const fn key(&self) -> KeyValue {
        match *self {
            Self::Pressed { key, .. } | Self::Released { key, .. } => key,
        }
    }

    /// The [`Modifiers`] held while this event was produced.
    #[must_use]
    pub const fn modifiers(&self) -> Modifiers {
        match *self {
            Self::Pressed { modifiers, .. } | Self::Released { modifiers, .. } => modifiers,
        }
    }

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, KEY_INPUT_MAGIC);
        put_u16(&mut out, 4, crate::ABI_VERSION_CURRENT_U16);
        let kind = match self {
            Self::Pressed { .. } => KIND_KEY_PRESSED,
            Self::Released { .. } => KIND_KEY_RELEASED,
        };
        put_u16(&mut out, 6, kind);
        put_u16(&mut out, 8, self.modifiers().to_bits());
        let (class, codepoint, named) = match self.key() {
            KeyValue::Char(c) => (KEY_CLASS_CHAR, c as u32, 0),
            KeyValue::Named(named) => (KEY_CLASS_NAMED, 0, named.code()),
        };
        put_u16(&mut out, 10, class);
        put_u32(&mut out, 12, codepoint);
        put_u16(&mut out, 16, named);
        // bytes 18..20 reserved, already zero.
        out
    }

    /// Decode `bytes` into a [`KeyInput`].
    ///
    /// Every field is validated; an invalid record is refused rather than
    /// interpreted (`AGENTS.md` §5.4 / §19.5 — fail closed on untrusted
    /// input).
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::BadMagic`] if the magic word does not match, if the reserved
    ///   field is non-zero, or if the unused key field for the record's class
    ///   is non-zero (a named code on a character record, or a codepoint on a
    ///   named record).
    /// * [`Errno::AbiVersionUnsupported`] if `version` is not
    ///   [`crate::ABI_VERSION_CURRENT`].
    /// * [`Errno::OutOfRange`] if `kind`, `key_class`, the modifier bits, the
    ///   named-key code, or the character scalar is not valid (an
    ///   unpaired-surrogate or out-of-range codepoint is not a `char`).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != KEY_INPUT_MAGIC {
            return Err(Errno::BadMagic);
        }
        if u32::from(read_u16(bytes, 4)) != crate::ABI_VERSION_CURRENT {
            return Err(Errno::AbiVersionUnsupported);
        }
        if read_u16(bytes, 18) != 0 {
            return Err(Errno::BadMagic);
        }
        let kind = read_u16(bytes, 6);
        let modifiers = Modifiers::from_bits(read_u16(bytes, 8))?;
        let class = read_u16(bytes, 10);
        let codepoint = read_u32(bytes, 12);
        let named = read_u16(bytes, 16);
        let key = match class {
            KEY_CLASS_CHAR => {
                if named != 0 {
                    return Err(Errno::BadMagic);
                }
                KeyValue::Char(char::from_u32(codepoint).ok_or(Errno::OutOfRange)?)
            }
            KEY_CLASS_NAMED => {
                if codepoint != 0 {
                    return Err(Errno::BadMagic);
                }
                KeyValue::Named(NamedKeyCode::from_code(named)?)
            }
            _ => return Err(Errno::OutOfRange),
        };
        match kind {
            KIND_KEY_PRESSED => Ok(Self::Pressed { key, modifiers }),
            KIND_KEY_RELEASED => Ok(Self::Released { key, modifiers }),
            _ => Err(Errno::OutOfRange),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        KeyInput, KeyValue, Modifiers, NamedKeyCode, PointerButtonCode, PointerInput, BUTTON_NONE,
        KEY_CLASS_CHAR, KEY_CLASS_NAMED, KEY_INPUT_MAGIC, KIND_KEY_PRESSED, KIND_KEY_RELEASED,
        KIND_MOVED, KIND_PRESSED, KIND_RELEASED, MOD_ALT, MOD_CTRL, MOD_META, MOD_SHIFT,
        POINTER_INPUT_MAGIC,
    };
    use crate::{Errno, ABI_VERSION_CURRENT};

    #[test]
    fn wire_size_is_twenty() {
        assert_eq!(PointerInput::WIRE_LEN, 20);
    }

    #[test]
    fn moved_round_trips() {
        let event = PointerInput::Moved { x: -17, y: 42 };
        let decoded = PointerInput::from_bytes(&event.to_le_bytes()).expect("valid record");
        assert_eq!(decoded, event);
    }

    #[test]
    fn press_and_release_round_trip_each_button() {
        for button in [
            PointerButtonCode::Primary,
            PointerButtonCode::Secondary,
            PointerButtonCode::Middle,
        ] {
            for event in [
                PointerInput::Pressed(button),
                PointerInput::Released(button),
            ] {
                let decoded = PointerInput::from_bytes(&event.to_le_bytes()).expect("valid record");
                assert_eq!(decoded, event);
            }
        }
    }

    #[test]
    fn rejects_short_buffer() {
        assert_eq!(
            PointerInput::from_bytes(&[0u8; 8]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = PointerInput::Moved { x: 1, y: 2 }.to_le_bytes();
        bytes[0] ^= 0xFF;
        assert_eq!(PointerInput::from_bytes(&bytes), Err(Errno::BadMagic));
    }

    #[test]
    fn rejects_bad_version() {
        let mut bytes = PointerInput::Moved { x: 1, y: 2 }.to_le_bytes();
        bytes[4] = 99;
        assert_eq!(
            PointerInput::from_bytes(&bytes),
            Err(Errno::AbiVersionUnsupported)
        );
    }

    #[test]
    fn rejects_nonzero_reserved() {
        let mut bytes = PointerInput::Moved { x: 1, y: 2 }.to_le_bytes();
        bytes[10] = 1;
        assert_eq!(PointerInput::from_bytes(&bytes), Err(Errno::BadMagic));
    }

    #[test]
    fn rejects_unknown_kind() {
        let mut bytes = PointerInput::Moved { x: 0, y: 0 }.to_le_bytes();
        bytes[6] = 9;
        assert_eq!(PointerInput::from_bytes(&bytes), Err(Errno::OutOfRange));
    }

    #[test]
    fn rejects_button_on_motion() {
        let mut bytes = PointerInput::Moved { x: 0, y: 0 }.to_le_bytes();
        // Low byte of the `button` field at offset 8: set it to Primary (1).
        bytes[8] = 1;
        assert_eq!(PointerInput::from_bytes(&bytes), Err(Errno::OutOfRange));
    }

    #[test]
    fn rejects_no_button_on_press() {
        // A well-formed press, then clear its button field to BUTTON_NONE (0).
        let mut bytes = PointerInput::Pressed(PointerButtonCode::Primary).to_le_bytes();
        bytes[8] = 0;
        assert_eq!(PointerInput::from_bytes(&bytes), Err(Errno::OutOfRange));
    }

    #[test]
    fn rejects_unknown_button_on_release() {
        let mut bytes = PointerInput::Released(PointerButtonCode::Middle).to_le_bytes();
        bytes[8] = 7;
        assert_eq!(PointerInput::from_bytes(&bytes), Err(Errno::OutOfRange));
    }

    #[test]
    fn rejects_coordinate_on_press() {
        let mut bytes = PointerInput::Pressed(PointerButtonCode::Primary).to_le_bytes();
        bytes[12] = 1;
        assert_eq!(PointerInput::from_bytes(&bytes), Err(Errno::BadMagic));
    }

    #[test]
    fn button_code_rejects_unknown() {
        assert_eq!(PointerButtonCode::from_code(0), Err(Errno::OutOfRange));
        assert_eq!(PointerButtonCode::from_code(4), Err(Errno::OutOfRange));
    }

    #[test]
    fn known_constants_are_frozen() {
        assert_eq!(POINTER_INPUT_MAGIC, u32::from_le_bytes(*b"PIN1"));
        assert_eq!(KIND_MOVED, 0);
        assert_eq!(KIND_PRESSED, 1);
        assert_eq!(KIND_RELEASED, 2);
        assert_eq!(BUTTON_NONE, 0);
        assert_eq!(PointerButtonCode::Primary.code(), 1);
        assert_eq!(PointerButtonCode::Secondary.code(), 2);
        assert_eq!(PointerButtonCode::Middle.code(), 3);
        // The version stamped into a record matches the crate's current ABI.
        assert_eq!(
            u32::from(crate::ABI_VERSION_CURRENT_U16),
            ABI_VERSION_CURRENT
        );
    }

    #[test]
    fn key_wire_size_is_twenty() {
        assert_eq!(KeyInput::WIRE_LEN, 20);
    }

    #[test]
    fn key_char_round_trips_with_modifiers() {
        let event = KeyInput::Pressed {
            key: KeyValue::Char('Q'),
            modifiers: Modifiers {
                shift: true,
                ctrl: true,
                ..Modifiers::default()
            },
        };
        let decoded = KeyInput::from_bytes(&event.to_le_bytes()).expect("valid record");
        assert_eq!(decoded, event);
        assert_eq!(decoded.key(), KeyValue::Char('Q'));
        assert!(decoded.modifiers().shift && decoded.modifiers().ctrl);
        assert!(!decoded.modifiers().alt && !decoded.modifiers().meta);
    }

    #[test]
    fn key_named_round_trips_for_press_and_release() {
        for code in [
            NamedKeyCode::Enter,
            NamedKeyCode::Escape,
            NamedKeyCode::Left,
            NamedKeyCode::F12,
        ] {
            for event in [
                KeyInput::Pressed {
                    key: KeyValue::Named(code),
                    modifiers: Modifiers::default(),
                },
                KeyInput::Released {
                    key: KeyValue::Named(code),
                    modifiers: Modifiers {
                        alt: true,
                        meta: true,
                        ..Modifiers::default()
                    },
                },
            ] {
                let decoded = KeyInput::from_bytes(&event.to_le_bytes()).expect("valid record");
                assert_eq!(decoded, event);
            }
        }
    }

    #[test]
    fn key_char_carries_non_ascii_scalar() {
        let event = KeyInput::Pressed {
            key: KeyValue::Char('é'),
            modifiers: Modifiers::default(),
        };
        let decoded = KeyInput::from_bytes(&event.to_le_bytes()).expect("valid record");
        assert_eq!(decoded.key(), KeyValue::Char('é'));
    }

    #[test]
    fn key_rejects_short_buffer() {
        assert_eq!(KeyInput::from_bytes(&[0u8; 8]), Err(Errno::BufferTooSmall));
    }

    #[test]
    fn key_rejects_bad_magic() {
        let mut bytes = KeyInput::Pressed {
            key: KeyValue::Char('a'),
            modifiers: Modifiers::default(),
        }
        .to_le_bytes();
        bytes[0] ^= 0xFF;
        assert_eq!(KeyInput::from_bytes(&bytes), Err(Errno::BadMagic));
    }

    #[test]
    fn key_rejects_bad_version() {
        let mut bytes = KeyInput::Pressed {
            key: KeyValue::Char('a'),
            modifiers: Modifiers::default(),
        }
        .to_le_bytes();
        bytes[4] = 99;
        assert_eq!(
            KeyInput::from_bytes(&bytes),
            Err(Errno::AbiVersionUnsupported)
        );
    }

    #[test]
    fn key_rejects_nonzero_reserved() {
        let mut bytes = KeyInput::Pressed {
            key: KeyValue::Char('a'),
            modifiers: Modifiers::default(),
        }
        .to_le_bytes();
        bytes[18] = 1;
        assert_eq!(KeyInput::from_bytes(&bytes), Err(Errno::BadMagic));
    }

    #[test]
    fn key_rejects_unknown_kind() {
        let mut bytes = KeyInput::Pressed {
            key: KeyValue::Char('a'),
            modifiers: Modifiers::default(),
        }
        .to_le_bytes();
        bytes[6] = 9;
        assert_eq!(KeyInput::from_bytes(&bytes), Err(Errno::OutOfRange));
    }

    #[test]
    fn key_rejects_unknown_modifier_bit() {
        let mut bytes = KeyInput::Pressed {
            key: KeyValue::Char('a'),
            modifiers: Modifiers::default(),
        }
        .to_le_bytes();
        // The high bits of the modifier field are not defined.
        bytes[8] = 0x10;
        assert_eq!(KeyInput::from_bytes(&bytes), Err(Errno::OutOfRange));
    }

    #[test]
    fn key_rejects_unknown_class() {
        let mut bytes = KeyInput::Pressed {
            key: KeyValue::Char('a'),
            modifiers: Modifiers::default(),
        }
        .to_le_bytes();
        bytes[10] = 9;
        assert_eq!(KeyInput::from_bytes(&bytes), Err(Errno::OutOfRange));
    }

    #[test]
    fn key_rejects_named_code_on_char_record() {
        // A well-formed char record, then plant a named code in the unused
        // field: the two key fields must never both be set.
        let mut bytes = KeyInput::Pressed {
            key: KeyValue::Char('a'),
            modifiers: Modifiers::default(),
        }
        .to_le_bytes();
        // Enter's wire code is 1.
        bytes[16] = 1;
        assert_eq!(KeyInput::from_bytes(&bytes), Err(Errno::BadMagic));
    }

    #[test]
    fn key_rejects_codepoint_on_named_record() {
        let mut bytes = KeyInput::Pressed {
            key: KeyValue::Named(NamedKeyCode::Enter),
            modifiers: Modifiers::default(),
        }
        .to_le_bytes();
        bytes[12] = 1;
        assert_eq!(KeyInput::from_bytes(&bytes), Err(Errno::BadMagic));
    }

    #[test]
    fn key_rejects_unknown_named_code() {
        let mut bytes = KeyInput::Pressed {
            key: KeyValue::Named(NamedKeyCode::Enter),
            modifiers: Modifiers::default(),
        }
        .to_le_bytes();
        bytes[16] = 99;
        assert_eq!(KeyInput::from_bytes(&bytes), Err(Errno::OutOfRange));
    }

    #[test]
    fn key_rejects_surrogate_codepoint() {
        // 0xD800 is an unpaired surrogate: not a valid Unicode scalar.
        let mut bytes = KeyInput::Pressed {
            key: KeyValue::Char('a'),
            modifiers: Modifiers::default(),
        }
        .to_le_bytes();
        bytes[12] = 0x00;
        bytes[13] = 0xD8;
        assert_eq!(KeyInput::from_bytes(&bytes), Err(Errno::OutOfRange));
    }

    #[test]
    fn key_constants_are_frozen() {
        assert_eq!(KEY_INPUT_MAGIC, u32::from_le_bytes(*b"KIN1"));
        assert_eq!(KIND_KEY_PRESSED, 1);
        assert_eq!(KIND_KEY_RELEASED, 2);
        assert_eq!(KEY_CLASS_CHAR, 0);
        assert_eq!(KEY_CLASS_NAMED, 1);
        assert_eq!((MOD_SHIFT, MOD_CTRL, MOD_ALT, MOD_META), (1, 2, 4, 8));
        assert_eq!(NamedKeyCode::Enter.code(), 1);
        assert_eq!(NamedKeyCode::F12.code(), 26);
    }

    #[test]
    fn named_key_code_round_trips_every_code() {
        for code in 1..=26u16 {
            let named = NamedKeyCode::from_code(code).expect("a defined named key");
            assert_eq!(named.code(), code);
        }
        assert_eq!(NamedKeyCode::from_code(0), Err(Errno::OutOfRange));
        assert_eq!(NamedKeyCode::from_code(27), Err(Errno::OutOfRange));
    }
}
