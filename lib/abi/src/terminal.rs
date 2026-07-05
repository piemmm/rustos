//! Terminal (text-console) geometry carried across the ABI.
//!
//! A [`TerminalSize`] is the character-cell grid of a text console: the number
//! of rows and columns a full-screen terminal program (a curses application
//! such as `top`) may draw into. It is reported by the `terminal_size` syscall
//! for the console backing a caller's standard stream.
//!
//! # What the kernel does and does not know
//!
//! The kernel reports a [`TerminalSize`] **only** for a console whose geometry
//! it actually knows — a framebuffer text console, whose cell grid is a
//! function of the panel resolution and the font. For a byte-stream console
//! (a UART / serial line) the true size of the *remote* terminal is
//! unknowable to the kernel: it is a property of whatever terminal emulator
//! sits at the far end of the wire, discoverable only by interrogating that
//! terminal at runtime. The kernel therefore never fabricates a size for such
//! a console; `terminal_size` fails closed and the conventional fallback (an
//! 80×24 grid) is applied by the client terminal library, where that policy
//! belongs — the kernel states only what it can attest.
//!
//! # Invariant
//!
//! A terminal always has at least one row and one column: a zero dimension is
//! not a representable grid. [`TerminalSize::new`] and [`TerminalSize::from_bytes`]
//! reject a zero `rows` or `cols` (fail closed), so an all-zero buffer can
//! never be mistaken for a valid geometry.

use crate::{Errno, Result};

/// The console read line discipline a program selects for its standard
/// input (the `stream_input_mode` syscall).
///
/// The mode says how the console treats the bytes a `stream_read` consumes
/// **on screen** — the discipline of the input side, chosen by the program
/// that owns the read:
///
/// * [`Cooked`](Self::Cooked) — the interactive default: the console echoes
///   what the user types (with the erase rub-out and CR-LF cooking of the
///   line discipline).
/// * [`Secret`](Self::Secret) — a credential read: echo is suppressed and
///   the console shows the shared `[input active...]` activity indicator
///   instead of the characters, so the operator sees progress but never the
///   secret (login's password prompt).
/// * [`Raw`](Self::Raw) — a full-screen program's read: echo is suppressed
///   and **nothing** is drawn by the console — the program (a curses viewer
///   such as `top`) paints its own display and a stray echo or indicator
///   would corrupt it.
///
/// The discriminant is the `u32` carried in the syscall's `mode` register;
/// `0` is reserved and never valid, so a zeroed register fails closed rather
/// than resolving to a mode the caller did not name.
#[repr(u32)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum InputMode {
    /// Echo on: the interactive line-editing default.
    Cooked = 1,
    /// Echo off, activity indicator on: a secret (password) read.
    Secret = 2,
    /// Echo off, nothing drawn: a full-screen (curses) program's read.
    Raw = 3,
}

impl InputMode {
    /// The discriminant carried on the wire.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Recover an [`InputMode`] from its wire discriminant.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::OutOfRange`] for any value that is not a defined
    /// mode (including the reserved `0`), so an unknown or zeroed register
    /// fails closed rather than being interpreted as a mode the caller did
    /// not name.
    pub const fn from_u32(value: u32) -> Result<Self> {
        match value {
            1 => Ok(Self::Cooked),
            2 => Ok(Self::Secret),
            3 => Ok(Self::Raw),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// Whether the console echoes the bytes a read consumes in this mode.
    /// Only the cooked (interactive) discipline echoes.
    #[must_use]
    pub const fn echoes(self) -> bool {
        matches!(self, Self::Cooked)
    }
}

/// On-wire byte length of a [`TerminalSize`]: two little-endian `u16`s.
pub const TERMINAL_SIZE_WIRE_LEN: usize = 4;

/// The character-cell dimensions of a text console.
///
/// Both dimensions are guaranteed non-zero by construction, so a
/// [`TerminalSize`] always denotes a drawable grid.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct TerminalSize {
    rows: u16,
    cols: u16,
}

impl TerminalSize {
    /// On-wire byte length (also available as the free constant
    /// [`TERMINAL_SIZE_WIRE_LEN`]).
    pub const WIRE_LEN: usize = TERMINAL_SIZE_WIRE_LEN;

    /// Construct a geometry of `rows` × `cols` character cells.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if either dimension is zero — a terminal with no
    /// rows or no columns is not a representable grid (fail closed).
    pub const fn new(rows: u16, cols: u16) -> Result<Self> {
        if rows == 0 || cols == 0 {
            return Err(Errno::OutOfRange);
        }
        Ok(Self { rows, cols })
    }

    /// The number of character rows.
    #[must_use]
    pub const fn rows(self) -> u16 {
        self.rows
    }

    /// The number of character columns.
    #[must_use]
    pub const fn cols(self) -> u16 {
        self.cols
    }

    /// Encode as little-endian bytes: `rows` then `cols`.
    #[must_use]
    pub const fn to_le_bytes(self) -> [u8; TERMINAL_SIZE_WIRE_LEN] {
        let [r0, r1] = self.rows.to_le_bytes();
        let [c0, c1] = self.cols.to_le_bytes();
        [r0, r1, c0, c1]
    }

    /// Decode from a byte slice.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] if `bytes` is not exactly
    ///   [`TERMINAL_SIZE_WIRE_LEN`] long — never silently truncating or
    ///   zero-extending a malformed input.
    /// * [`Errno::OutOfRange`] if the decoded grid has a zero dimension.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != TERMINAL_SIZE_WIRE_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        let rows = u16::from_le_bytes([bytes[0], bytes[1]]);
        let cols = u16::from_le_bytes([bytes[2], bytes[3]]);
        Self::new(rows, cols)
    }
}

#[cfg(test)]
mod tests {
    use super::{InputMode, TerminalSize, TERMINAL_SIZE_WIRE_LEN};
    use crate::Errno;

    #[test]
    fn input_mode_discriminants_are_frozen() {
        // The discriminants are the on-wire mode values; do not renumber.
        assert_eq!(InputMode::Cooked.as_u32(), 1);
        assert_eq!(InputMode::Secret.as_u32(), 2);
        assert_eq!(InputMode::Raw.as_u32(), 3);
    }

    #[test]
    fn input_mode_round_trips_through_its_discriminant() {
        for mode in [InputMode::Cooked, InputMode::Secret, InputMode::Raw] {
            assert_eq!(InputMode::from_u32(mode.as_u32()), Ok(mode));
        }
    }

    #[test]
    fn input_mode_rejects_reserved_and_unknown_values() {
        // 0 is reserved so a zeroed register fails closed, and every value
        // past the defined set is rejected rather than guessed.
        assert_eq!(InputMode::from_u32(0), Err(Errno::OutOfRange));
        assert_eq!(InputMode::from_u32(4), Err(Errno::OutOfRange));
        assert_eq!(InputMode::from_u32(u32::MAX), Err(Errno::OutOfRange));
    }

    #[test]
    fn only_the_cooked_mode_echoes() {
        assert!(InputMode::Cooked.echoes());
        assert!(!InputMode::Secret.echoes());
        assert!(!InputMode::Raw.echoes());
    }

    #[test]
    fn round_trips_through_bytes() {
        let size = TerminalSize::new(24, 80).unwrap();
        assert_eq!(size.rows(), 24);
        assert_eq!(size.cols(), 80);
        let bytes = size.to_le_bytes();
        assert_eq!(bytes, [24, 0, 80, 0]);
        assert_eq!(TerminalSize::from_bytes(&bytes), Ok(size));
    }

    #[test]
    fn large_dimensions_round_trip() {
        // A big display grid is legal; there is no upper clamp.
        let size = TerminalSize::new(4096, 512).unwrap();
        assert_eq!(TerminalSize::from_bytes(&size.to_le_bytes()), Ok(size));
    }

    #[test]
    fn zero_dimension_is_rejected_fail_closed() {
        assert_eq!(TerminalSize::new(0, 80), Err(Errno::OutOfRange));
        assert_eq!(TerminalSize::new(24, 0), Err(Errno::OutOfRange));
        assert_eq!(TerminalSize::new(0, 0), Err(Errno::OutOfRange));
        // An all-zero buffer decodes to a rejected grid, not a valid one.
        assert_eq!(
            TerminalSize::from_bytes(&[0u8; TERMINAL_SIZE_WIRE_LEN]),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn from_bytes_rejects_wrong_length_fail_closed() {
        assert_eq!(TerminalSize::from_bytes(&[]), Err(Errno::LengthOutOfRange));
        assert_eq!(
            TerminalSize::from_bytes(&[24, 0, 80]),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            TerminalSize::from_bytes(&[24, 0, 80, 0, 0]),
            Err(Errno::LengthOutOfRange)
        );
    }
}
