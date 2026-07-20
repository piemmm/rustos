//! Wire-level pointer input event delivered over the kernel input channel.
//!
//! A pointing device reports motion, presses, and releases. On a running
//! system those reports reach the user-space desktop as a stream of framed
//! records over a capability-checked kernel input channel; this module fixes
//! the *contract* of one such record, [`PointerInput`]. The kernel never hands a parser the device's structure: it hands a
//! buffer, and [`PointerInput::from_bytes`] validates every field and fails
//! closed.
//!
//! The record is **device-resolved but screen-independent**: motion is the
//! relative displacement the device reported ([`MovedBy`](PointerInput::MovedBy)),
//! and a button edge names a *resolved* button (primary / secondary /
//! middle), never a raw platform keycode. Turning this stream into an
//! absolute on-screen pointer position — accumulating displacements and
//! clamping to the display extent — is the **seat owner's** policy: only the
//! desktop session that owns the compositor knows the screen's pixel size
//! (and its runtime scale), so the position lives there, never in a driver.
//! An input driver therefore needs no display-geometry authority at all —
//! it decodes its device and injects, and the kernel seat channel is a dumb,
//! validated pipe. (The desktop's `no_std` GUI vocabulary — `lib/input`'s
//! absolute `PointerMoved` — is what the seat owner produces *after* that
//! accumulation.)
//!
//! Keyboard input is modelled alongside the pointer by [`KeyInput`]: the
//! *desktop-level* key event the window manager delivers to the focused
//! surface — a produced [`KeyValue`] (a Unicode character, or a named
//! non-character key) plus the [`Modifiers`] held while it was produced. As
//! with the pointer, this is **not** the device-level report: turning raw
//! platform keycodes and a keyboard layout into a produced character is policy
//! that lives above the driver, not a second copy of the same data.
//! [`KeyInput::from_bytes`] validates every field and fails closed on
//! untrusted input.
//!
//! # Relationship to the driver input ABI
//!
//! This is **not** a duplicate of [`crate::driver::input::InputEvent`]. That type is the *device-level* contract an input
//! driver reports across the [`Input`](crate::driver::input::Input) driver
//! trait: single-axis pointer *deltas*, scroll ticks, and platform keycodes,
//! one event per axis or edge. [`PointerInput`] is the *seat-channel* record
//! an input-driver process injects (`pointer_inject`): button keycodes are
//! resolved to the closed button set, and a scroll wheel becomes a
//! [`Scrolled`](PointerInput::Scrolled) tick record now that the desktop
//! scrollbar consumes it. [`PointerInput::from_device_event`] is the one
//! spelling of that mapping, shared by every pointer-input driver.
//!
//! # Wire layout
//!
//! A record is exactly [`PointerInput::WIRE_LEN`] bytes, little-endian:
//!
//! | offset | size | field      | meaning                                  |
//! |-------:|-----:|------------|------------------------------------------|
//! |      0 |    4 | `magic`    | [`POINTER_INPUT_MAGIC`]                   |
//! |      4 |    2 | `version`  | ABI version ([`crate::ABI_VERSION_CURRENT`]) |
//! |      6 |    2 | `kind`     | [`KIND_MOVED_BY`] / [`KIND_PRESSED`] / [`KIND_RELEASED`] / [`KIND_SCROLLED`] |
//! |      8 |    2 | `button`   | [`BUTTON_NONE`] for motion/scroll, else a button code |
//! |     10 |    2 | `reserved` | must be zero                             |
//! |     12 |    4 | `dx`       | signed x displacement / scroll ticks     |
//! |     16 |    4 | `dy`       | signed y displacement / scroll ticks     |
//!
//! The displacement fields carry the reported motion for a
//! [`MovedBy`](PointerInput::MovedBy) record, the signed wheel ticks for a
//! [`Scrolled`](PointerInput::Scrolled) record, and must be zero for a press
//! or release (a real pointing device reports motion separately from clicks,
//! and the seat owner applies a button at the position its accumulated motion
//! established — the same model as `lib/input`). The `button` field is
//! [`BUTTON_NONE`] for motion and scroll and a valid button code for a press
//! or release. Any other combination is wire corruption and is refused.

use crate::driver::input::{
    InputEvent, InputEventKind, AXIS_X, AXIS_Y, POINTER_BUTTON_CODE_BASE, POINTER_BUTTON_COUNT,
};
use crate::le::{put_i32, put_u16, put_u32, read_i32, read_u16, read_u32};
use crate::Errno;

/// Magic number identifying an `abi-v1` pointer input record (`"PIN1"`
/// little-endian).
pub const POINTER_INPUT_MAGIC: u32 = u32::from_le_bytes(*b"PIN1");

/// `kind` code for a relative pointer move.
pub const KIND_MOVED_BY: u16 = 0;
/// `kind` code for a pointer-button press.
pub const KIND_PRESSED: u16 = 1;
/// `kind` code for a pointer-button release.
pub const KIND_RELEASED: u16 = 2;
/// `kind` code for a scroll-wheel tick.
pub const KIND_SCROLLED: u16 = 3;

/// `button` code carried by a [`MovedBy`](PointerInput::MovedBy) or
/// [`Scrolled`](PointerInput::Scrolled) record: no button is involved in a
/// motion or scroll event.
pub const BUTTON_NONE: u16 = 0;

/// The pointer button a press or release record names.
///
/// A `#[repr(u16)]` enum so the wire code is exactly the integer carried in
/// the record's `button` field. The discriminants are part of the frozen
/// pointer-input ABI; [`PointerButtonCode::from_code`] rejects any other
/// value rather than guessing (validate every input).
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
/// The type makes illegal states unrepresentable: a
/// [`MovedBy`](Self::MovedBy) carries a displacement and no button, while a
/// [`Pressed`](Self::Pressed) / [`Released`](Self::Released) carries a button
/// and no displacement. [`from_bytes`](Self::from_bytes) is the only way to
/// build one from untrusted bytes and validates the whole record before
/// returning it.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum PointerInput {
    /// The pointer moved by a relative displacement, in the device's
    /// count units (positive x rightward, positive y downward — the
    /// `evdev` orientation). The seat owner accumulates displacements
    /// into its absolute, extent-clamped screen position.
    MovedBy {
        /// Signed x displacement.
        dx: i32,
        /// Signed y displacement.
        dy: i32,
    },
    /// A pointer button went down at the current pointer position.
    Pressed(PointerButtonCode),
    /// A pointer button came up at the current pointer position.
    Released(PointerButtonCode),
    /// The scroll wheel turned by a relative number of ticks, in the
    /// device's detent units (positive x toward the logical end, positive y
    /// downward — the `evdev` orientation). A scroll acts at the current
    /// pointer position, exactly as a button does; the seat owner routes it
    /// to the viewport under that position.
    Scrolled {
        /// Signed horizontal scroll ticks.
        dx: i32,
        /// Signed vertical scroll ticks.
        dy: i32,
    },
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
        let (kind, button, dx, dy) = match *self {
            Self::MovedBy { dx, dy } => (KIND_MOVED_BY, BUTTON_NONE, dx, dy),
            Self::Pressed(button) => (KIND_PRESSED, button.code(), 0, 0),
            Self::Released(button) => (KIND_RELEASED, button.code(), 0, 0),
            Self::Scrolled { dx, dy } => (KIND_SCROLLED, BUTTON_NONE, dx, dy),
        };
        put_u16(&mut out, 6, kind);
        put_u16(&mut out, 8, button);
        // bytes 10..12 reserved, already zero.
        put_i32(&mut out, 12, dx);
        put_i32(&mut out, 16, dy);
        out
    }

    /// Decode `bytes` into a [`PointerInput`].
    ///
    /// Every field is validated; an invalid record is refused rather than
    /// interpreted (fail closed on untrusted
    /// input).
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::BadMagic`] if the magic word does not match, or if the
    ///   reserved field is non-zero, or if a press/release record carries a
    ///   non-zero displacement (a displacement belongs to a motion record
    ///   only).
    /// * [`Errno::AbiVersionUnsupported`] if `version` is not
    ///   [`crate::ABI_VERSION_CURRENT`].
    /// * [`Errno::OutOfRange`] if `kind` is not a defined kind, or if the
    ///   `button` field is inconsistent with the kind (a button code on a
    ///   motion or scroll record, or [`BUTTON_NONE`]/an unknown code on a
    ///   press or release).
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
        let dx = read_i32(bytes, 12);
        let dy = read_i32(bytes, 16);
        match kind {
            KIND_MOVED_BY => {
                if button != BUTTON_NONE {
                    return Err(Errno::OutOfRange);
                }
                Ok(Self::MovedBy { dx, dy })
            }
            KIND_SCROLLED => {
                if button != BUTTON_NONE {
                    return Err(Errno::OutOfRange);
                }
                Ok(Self::Scrolled { dx, dy })
            }
            KIND_PRESSED | KIND_RELEASED => {
                if dx != 0 || dy != 0 {
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

    /// Translate one device-level [`InputEvent`] into the seat-channel
    /// record an input driver injects, or [`None`] when the event carries
    /// no pointer record.
    ///
    /// The one shared spelling of the device→seat pointer mapping, so the
    /// virtio and USB HID driver processes can never diverge:
    ///
    /// * a [`Pointer`](InputEventKind::Pointer) axis delta becomes a
    ///   single-axis [`MovedBy`](Self::MovedBy);
    /// * a [`Key`](InputEventKind::Key) edge whose code is one of the
    ///   [`POINTER_BUTTON_CODE_BASE`] button codes becomes a
    ///   [`Pressed`](Self::Pressed) / [`Released`](Self::Released) of the
    ///   resolved button;
    /// * a [`Scroll`](InputEventKind::Scroll) axis tick becomes a
    ///   single-axis [`Scrolled`](Self::Scrolled);
    /// * everything else — keyboard keys (a keyboard producer's job), an
    ///   unknown axis, or a non-edge button value (a repeat) — produces no
    ///   record rather than a guessed one (fail closed, never fabricate
    ///   input).
    #[must_use]
    pub fn from_device_event(event: &InputEvent) -> Option<Self> {
        match event.kind {
            InputEventKind::Pointer => match event.code {
                AXIS_X => Some(Self::MovedBy {
                    dx: event.value,
                    dy: 0,
                }),
                AXIS_Y => Some(Self::MovedBy {
                    dx: 0,
                    dy: event.value,
                }),
                _ => None,
            },
            InputEventKind::Key => {
                let offset = event.code.checked_sub(POINTER_BUTTON_CODE_BASE)?;
                if offset >= POINTER_BUTTON_COUNT {
                    return None;
                }
                // The wire button codes are 1-based (`BUTTON_NONE` is 0),
                // the device codes 0-based from the base; `from_code`
                // keeps the closed set authoritative.
                let button = PointerButtonCode::from_code(offset + 1).ok()?;
                match event.value {
                    1 => Some(Self::Pressed(button)),
                    0 => Some(Self::Released(button)),
                    _ => None,
                }
            }
            InputEventKind::Scroll => match event.code {
                AXIS_X => Some(Self::Scrolled {
                    dx: event.value,
                    dy: 0,
                }),
                AXIS_Y => Some(Self::Scrolled {
                    dx: 0,
                    dy: event.value,
                }),
                _ => None,
            },
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
/// undefined bit rather than silently dropping it.
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
    /// not define (validate every input, fail closed).
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
/// [`NamedKeyCode::from_code`] rejects any other value rather than guessing. Character-producing keys (letters, digits, punctuation,
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
/// The type makes illegal states unrepresentable: a
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
    /// interpreted (fail closed on untrusted
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
        KIND_MOVED_BY, KIND_PRESSED, KIND_RELEASED, KIND_SCROLLED, MOD_ALT, MOD_CTRL, MOD_META,
        MOD_SHIFT, POINTER_INPUT_MAGIC,
    };
    use crate::driver::input::{
        InputEvent, InputEventKind, AXIS_X, AXIS_Y, POINTER_BUTTON_CODE_BASE,
    };
    use crate::{Errno, ABI_VERSION_CURRENT};

    #[test]
    fn wire_size_is_twenty() {
        assert_eq!(PointerInput::WIRE_LEN, 20);
    }

    #[test]
    fn moved_round_trips() {
        let event = PointerInput::MovedBy { dx: -17, dy: 42 };
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
        let mut bytes = PointerInput::MovedBy { dx: 1, dy: 2 }.to_le_bytes();
        bytes[0] ^= 0xFF;
        assert_eq!(PointerInput::from_bytes(&bytes), Err(Errno::BadMagic));
    }

    #[test]
    fn rejects_bad_version() {
        let mut bytes = PointerInput::MovedBy { dx: 1, dy: 2 }.to_le_bytes();
        bytes[4] = 99;
        assert_eq!(
            PointerInput::from_bytes(&bytes),
            Err(Errno::AbiVersionUnsupported)
        );
    }

    #[test]
    fn rejects_nonzero_reserved() {
        let mut bytes = PointerInput::MovedBy { dx: 1, dy: 2 }.to_le_bytes();
        bytes[10] = 1;
        assert_eq!(PointerInput::from_bytes(&bytes), Err(Errno::BadMagic));
    }

    #[test]
    fn rejects_unknown_kind() {
        let mut bytes = PointerInput::MovedBy { dx: 0, dy: 0 }.to_le_bytes();
        bytes[6] = 9;
        assert_eq!(PointerInput::from_bytes(&bytes), Err(Errno::OutOfRange));
    }

    #[test]
    fn rejects_button_on_motion() {
        let mut bytes = PointerInput::MovedBy { dx: 0, dy: 0 }.to_le_bytes();
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
    fn rejects_displacement_on_press() {
        let mut bytes = PointerInput::Pressed(PointerButtonCode::Primary).to_le_bytes();
        bytes[12] = 1;
        assert_eq!(PointerInput::from_bytes(&bytes), Err(Errno::BadMagic));
    }

    #[test]
    fn scroll_round_trips_with_signed_ticks() {
        let event = PointerInput::Scrolled { dx: -3, dy: 5 };
        let decoded = PointerInput::from_bytes(&event.to_le_bytes()).expect("valid record");
        assert_eq!(decoded, event);
    }

    #[test]
    fn rejects_button_on_scroll() {
        // A well-formed scroll record, then plant a button code: a scroll
        // carries no button, exactly like a motion record.
        let mut bytes = PointerInput::Scrolled { dx: 0, dy: 1 }.to_le_bytes();
        bytes[8] = 1;
        assert_eq!(PointerInput::from_bytes(&bytes), Err(Errno::OutOfRange));
    }

    /// One device event per axis maps onto a single-axis displacement.
    #[test]
    fn device_axis_deltas_map_to_single_axis_moves() {
        let x = InputEvent {
            kind: InputEventKind::Pointer,
            reserved0: 0,
            code: AXIS_X,
            value: -6,
        };
        let y = InputEvent {
            kind: InputEventKind::Pointer,
            reserved0: 0,
            code: AXIS_Y,
            value: 11,
        };
        assert_eq!(
            PointerInput::from_device_event(&x),
            Some(PointerInput::MovedBy { dx: -6, dy: 0 })
        );
        assert_eq!(
            PointerInput::from_device_event(&y),
            Some(PointerInput::MovedBy { dx: 0, dy: 11 })
        );
    }

    /// The three `evdev` button codes resolve to the closed button set,
    /// press and release alike.
    #[test]
    fn device_button_edges_resolve_each_button() {
        for (offset, button) in [
            (0, PointerButtonCode::Primary),
            (1, PointerButtonCode::Secondary),
            (2, PointerButtonCode::Middle),
        ] {
            let mut event = InputEvent {
                kind: InputEventKind::Key,
                reserved0: 0,
                code: POINTER_BUTTON_CODE_BASE + offset,
                value: 1,
            };
            assert_eq!(
                PointerInput::from_device_event(&event),
                Some(PointerInput::Pressed(button))
            );
            event.value = 0;
            assert_eq!(
                PointerInput::from_device_event(&event),
                Some(PointerInput::Released(button))
            );
        }
    }

    /// One device scroll event per axis maps onto a single-axis tick record.
    #[test]
    fn device_scroll_ticks_map_to_single_axis_scrolls() {
        let x = InputEvent {
            kind: InputEventKind::Scroll,
            reserved0: 0,
            code: AXIS_X,
            value: 2,
        };
        let y = InputEvent {
            kind: InputEventKind::Scroll,
            reserved0: 0,
            code: AXIS_Y,
            value: -1,
        };
        assert_eq!(
            PointerInput::from_device_event(&x),
            Some(PointerInput::Scrolled { dx: 2, dy: 0 })
        );
        assert_eq!(
            PointerInput::from_device_event(&y),
            Some(PointerInput::Scrolled { dx: 0, dy: -1 })
        );
    }

    /// Keyboard keys, unknown axes, out-of-set buttons, and repeat values
    /// produce no record (fail closed, never fabricate).
    #[test]
    fn device_events_outside_the_pointer_vocabulary_produce_nothing() {
        let cases = [
            // A keyboard key (`KEY_A` = 30) is the keyboard producer's job.
            InputEvent {
                kind: InputEventKind::Key,
                reserved0: 0,
                code: 30,
                value: 1,
            },
            // A code past the modelled button set (`BTN_SIDE` = 0x113).
            InputEvent {
                kind: InputEventKind::Key,
                reserved0: 0,
                code: POINTER_BUTTON_CODE_BASE + 3,
                value: 1,
            },
            // A button repeat carries no edge.
            InputEvent {
                kind: InputEventKind::Key,
                reserved0: 0,
                code: POINTER_BUTTON_CODE_BASE,
                value: 2,
            },
            // A pointer axis this vocabulary does not model.
            InputEvent {
                kind: InputEventKind::Pointer,
                reserved0: 0,
                code: 2,
                value: 1,
            },
            // A scroll axis this vocabulary does not model.
            InputEvent {
                kind: InputEventKind::Scroll,
                reserved0: 0,
                code: 2,
                value: -1,
            },
        ];
        for event in cases {
            assert_eq!(PointerInput::from_device_event(&event), None);
        }
    }

    #[test]
    fn button_code_rejects_unknown() {
        assert_eq!(PointerButtonCode::from_code(0), Err(Errno::OutOfRange));
        assert_eq!(PointerButtonCode::from_code(4), Err(Errno::OutOfRange));
    }

    #[test]
    fn known_constants_are_frozen() {
        assert_eq!(POINTER_INPUT_MAGIC, u32::from_le_bytes(*b"PIN1"));
        assert_eq!(KIND_MOVED_BY, 0);
        assert_eq!(KIND_PRESSED, 1);
        assert_eq!(KIND_RELEASED, 2);
        assert_eq!(KIND_SCROLLED, 3);
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
