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
//! Keyboard input is deliberately **not** modelled here, exactly as it is not
//! modelled in `lib/input`: the desktop tracks *which* surface owns the
//! keyboard, but the key encoding is a separate ABI concern that is not
//! invented in this layer (`AGENTS.md` §2.4 — no interface creep).
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

#[cfg(test)]
mod tests {
    use super::{
        PointerButtonCode, PointerInput, BUTTON_NONE, KIND_MOVED, KIND_PRESSED, KIND_RELEASED,
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
}
