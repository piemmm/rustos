//! [`TermType`] — the closed, versioned set of recognised `TERM` values.
//!
//! Adding a terminal is a data entry (a new variant plus its [`Capabilities`]
//! record and a capability test), not new control flow (`plans/CURSES.md` §1).
//! The set is frozen with the same discipline as any other ABI surface.

use crate::capabilities::Capabilities;

/// A terminal TAIRiX recognises by its `TERM` value.
///
/// [`TermType::Dumb`] and [`TermType::Vt100`] are the fail-closed fallbacks
/// (`plans/CURSES.md` §1): an unknown or missing `TERM` degrades to `Dumb`, and
/// `Vt100` is the minimal cursor-addressable baseline.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TermType {
    /// `xterm` — the baseline xterm with the 16 ANSI colours.
    Xterm,
    /// `xterm-color` — xterm advertising colour support.
    XtermColor,
    /// `xterm-16color` — xterm with the full 16-colour palette.
    Xterm16Color,
    /// `xterm-256color` — xterm with the 256-colour palette and the modern
    /// xterm feature set (alt-screen, mouse, bracketed paste).
    Xterm256Color,
    /// `alacritty` — the Alacritty GPU terminal (24-bit truecolour).
    Alacritty,
    /// `xterm-kitty` — the kitty terminal (24-bit truecolour).
    XtermKitty,
    /// `dumb` — no cursor addressing, no colour; the safe fallback.
    Dumb,
    /// `vt100` — the DEC VT100, the minimal cursor-addressable baseline.
    Vt100,
    /// `vt220` — the DEC VT220 (VT100 plus editing and function keys).
    Vt220,
}

impl TermType {
    /// Every [`TermType`] in declaration order, for exhaustive iteration in
    /// tests and callers that enumerate the database.
    pub const ALL: [TermType; 9] = [
        TermType::Xterm,
        TermType::XtermColor,
        TermType::Xterm16Color,
        TermType::Xterm256Color,
        TermType::Alacritty,
        TermType::XtermKitty,
        TermType::Dumb,
        TermType::Vt100,
        TermType::Vt220,
    ];

    /// The canonical `TERM` string for this terminal.
    ///
    /// This is the inverse of [`from_term`] over the recognised set:
    /// `from_term(t.term_name()) == t` for every [`TermType`].
    #[must_use]
    pub const fn term_name(self) -> &'static str {
        match self {
            TermType::Xterm => "xterm",
            TermType::XtermColor => "xterm-color",
            TermType::Xterm16Color => "xterm-16color",
            TermType::Xterm256Color => "xterm-256color",
            TermType::Alacritty => "alacritty",
            TermType::XtermKitty => "xterm-kitty",
            TermType::Dumb => "dumb",
            TermType::Vt100 => "vt100",
            TermType::Vt220 => "vt220",
        }
    }

    /// The compiled-in [`Capabilities`] record for this terminal.
    #[must_use]
    pub const fn capabilities(self) -> Capabilities {
        Capabilities::for_term(self)
    }
}

/// Resolve an untrusted `TERM` string to a [`TermType`].
///
/// The match is exact over the recognised set ([`TermType::ALL`]). An unknown
/// or empty value fails closed to [`TermType::Dumb`]
/// rather than guessing a richer terminal. Parsing `TERM` never triggers a file
/// read: the database is compiled in.
#[must_use]
pub fn from_term(term: &str) -> TermType {
    match term {
        "xterm" => TermType::Xterm,
        "xterm-color" => TermType::XtermColor,
        "xterm-16color" => TermType::Xterm16Color,
        "xterm-256color" => TermType::Xterm256Color,
        "alacritty" => TermType::Alacritty,
        "xterm-kitty" => TermType::XtermKitty,
        "vt100" => TermType::Vt100,
        "vt220" => TermType::Vt220,
        // "dumb", an empty string, and every unrecognised value fail closed
        // to the safe baseline.
        _ => TermType::Dumb,
    }
}
