//! The function, editing, and keypad keys, as a typed [`Key`] vocabulary.
//!
//! A terminal sends a *named* key — a function key, or an editing key such as
//! Home or Page Down — as an escape sequence rather than a printable
//! character. xterm and the VT220 use two encodings: the `SS3` form (`ESC O P`
//! for `F1`) and the `CSI … ~` form (`ESC [ 3 ~` for Delete). This module names
//! each key once so the emitter and the parser share that one definition: [`Key::ss3_final`] and [`Key::tilde_param`] give the
//! encoding the emitter writes, and the parser recognises both the canonical
//! form and the common alternates, mapping them back to the same [`Key`].
//!
//! The arrow keys are *not* here: in normal cursor mode they are exactly the
//! `CSI A`…`CSI D` cursor-movement sequences, so they are carried by the
//! [`crate::Op`] cursor-movement variants the database already references
//! (`lib/termcap`'s `ArrowKeys`), not duplicated as a second representation.

/// A named (non-printable) key a terminal reports as an escape sequence.
///
/// The function keys `F1`…`F12` and the six editing keys are the closed set
/// `lib/termcap` records as capability facts; their byte sequences live here in
/// the shared vocabulary so the curses input decoder reads them through the one
/// [`crate::Parser`] (`plans/CURSES.md` §C4).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Key {
    /// Function key `F1` (`ESC O P`).
    F1,
    /// Function key `F2` (`ESC O Q`).
    F2,
    /// Function key `F3` (`ESC O R`).
    F3,
    /// Function key `F4` (`ESC O S`).
    F4,
    /// Function key `F5` (`CSI 15 ~`).
    F5,
    /// Function key `F6` (`CSI 17 ~`).
    F6,
    /// Function key `F7` (`CSI 18 ~`).
    F7,
    /// Function key `F8` (`CSI 19 ~`).
    F8,
    /// Function key `F9` (`CSI 20 ~`).
    F9,
    /// Function key `F10` (`CSI 21 ~`).
    F10,
    /// Function key `F11` (`CSI 23 ~`).
    F11,
    /// Function key `F12` (`CSI 24 ~`).
    F12,
    /// Home (`CSI 1 ~`; also `CSI H` / `ESC O H`).
    Home,
    /// Insert (`CSI 2 ~`).
    Insert,
    /// Delete (`CSI 3 ~`).
    Delete,
    /// End (`CSI 4 ~`; also `CSI F` / `ESC O F`).
    End,
    /// Page Up (`CSI 5 ~`).
    PageUp,
    /// Page Down (`CSI 6 ~`).
    PageDown,
}

impl Key {
    /// Every [`Key`] in declaration order, for exhaustive iteration in tests
    /// and key tables.
    pub const ALL: [Key; 18] = [
        Key::F1,
        Key::F2,
        Key::F3,
        Key::F4,
        Key::F5,
        Key::F6,
        Key::F7,
        Key::F8,
        Key::F9,
        Key::F10,
        Key::F11,
        Key::F12,
        Key::Home,
        Key::Insert,
        Key::Delete,
        Key::End,
        Key::PageUp,
        Key::PageDown,
    ];

    /// The `SS3` final byte (`ESC O <byte>`) this key sends, if it has an `SS3`
    /// encoding.
    ///
    /// Only `F1`…`F4` (and the application-mode Home/End alternates the parser
    /// also accepts) use `SS3`; every other key returns `None` and is encoded
    /// with [`Key::tilde_param`].
    #[must_use]
    pub const fn ss3_final(self) -> Option<u8> {
        match self {
            Key::F1 => Some(b'P'),
            Key::F2 => Some(b'Q'),
            Key::F3 => Some(b'R'),
            Key::F4 => Some(b'S'),
            _ => None,
        }
    }

    /// The `CSI <n> ~` parameter this key sends, if it has a `~` encoding.
    ///
    /// `F1`…`F4` have no `~` form (they return `None`); every other key does.
    #[must_use]
    pub const fn tilde_param(self) -> Option<u16> {
        match self {
            Key::F1 | Key::F2 | Key::F3 | Key::F4 => None,
            Key::F5 => Some(15),
            Key::F6 => Some(17),
            Key::F7 => Some(18),
            Key::F8 => Some(19),
            Key::F9 => Some(20),
            Key::F10 => Some(21),
            Key::F11 => Some(23),
            Key::F12 => Some(24),
            Key::Home => Some(1),
            Key::Insert => Some(2),
            Key::Delete => Some(3),
            Key::End => Some(4),
            Key::PageUp => Some(5),
            Key::PageDown => Some(6),
        }
    }

    /// The [`Key`] for a `CSI <param> ~` sequence, or `None` if `param` is not
    /// a recognised key (fail closed).
    #[must_use]
    pub const fn from_tilde_param(param: u16) -> Option<Key> {
        match param {
            1 => Some(Key::Home),
            2 => Some(Key::Insert),
            3 => Some(Key::Delete),
            4 => Some(Key::End),
            5 => Some(Key::PageUp),
            6 => Some(Key::PageDown),
            15 => Some(Key::F5),
            17 => Some(Key::F6),
            18 => Some(Key::F7),
            19 => Some(Key::F8),
            20 => Some(Key::F9),
            21 => Some(Key::F10),
            23 => Some(Key::F11),
            24 => Some(Key::F12),
            _ => None,
        }
    }

    /// The [`Key`] for an `SS3` (`ESC O <byte>`) final byte, or `None` if the
    /// byte is not a recognised key.
    ///
    /// Accepts the `F1`…`F4` finals plus the application-mode `H` (Home) and
    /// `F` (End) alternates xterm sends in keypad-application mode.
    #[must_use]
    pub const fn from_ss3_final(byte: u8) -> Option<Key> {
        match byte {
            b'P' => Some(Key::F1),
            b'Q' => Some(Key::F2),
            b'R' => Some(Key::F3),
            b'S' => Some(Key::F4),
            b'H' => Some(Key::Home),
            b'F' => Some(Key::End),
            _ => None,
        }
    }
}
