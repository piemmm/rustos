//! The colour models of the SGR vocabulary.
//!
//! ANSI / xterm terminals address colour three ways, and this module names all
//! three so the emitter and parser share one definition:
//!
//! * the 16 [`BasicColor`]s (the eight ECMA-48 colours and their bright
//!   variants), selected by the SGR codes `30..=37` / `90..=97` (foreground)
//!   and `40..=47` / `100..=107` (background);
//! * the 256-colour palette, selected by `38;5;n` / `48;5;n`;
//! * 24-bit truecolour, selected by `38;2;r;g;b` / `48;2;r;g;b`.
//!
//! Each [`Color`] variant has exactly one canonical encoding, so a colour
//! emitted by [`crate::encode_into`] parses back to the identical [`Color`].

/// One of the sixteen ANSI named colours: the eight base colours and their
/// eight bright counterparts.
///
/// The discriminant is the palette index `0..=15`, so [`BasicColor::index`]
/// and [`BasicColor::from_index`] are total round-trips over that range.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum BasicColor {
    /// Palette index 0.
    Black = 0,
    /// Palette index 1.
    Red = 1,
    /// Palette index 2.
    Green = 2,
    /// Palette index 3.
    Yellow = 3,
    /// Palette index 4.
    Blue = 4,
    /// Palette index 5.
    Magenta = 5,
    /// Palette index 6.
    Cyan = 6,
    /// Palette index 7.
    White = 7,
    /// Palette index 8 (bright black / grey).
    BrightBlack = 8,
    /// Palette index 9.
    BrightRed = 9,
    /// Palette index 10.
    BrightGreen = 10,
    /// Palette index 11.
    BrightYellow = 11,
    /// Palette index 12.
    BrightBlue = 12,
    /// Palette index 13.
    BrightMagenta = 13,
    /// Palette index 14.
    BrightCyan = 14,
    /// Palette index 15.
    BrightWhite = 15,
}

impl BasicColor {
    /// Every [`BasicColor`] in palette order, for exhaustive iteration in
    /// tests and capability tables.
    pub const ALL: [BasicColor; 16] = [
        BasicColor::Black,
        BasicColor::Red,
        BasicColor::Green,
        BasicColor::Yellow,
        BasicColor::Blue,
        BasicColor::Magenta,
        BasicColor::Cyan,
        BasicColor::White,
        BasicColor::BrightBlack,
        BasicColor::BrightRed,
        BasicColor::BrightGreen,
        BasicColor::BrightYellow,
        BasicColor::BrightBlue,
        BasicColor::BrightMagenta,
        BasicColor::BrightCyan,
        BasicColor::BrightWhite,
    ];

    /// The palette index `0..=15` of this colour.
    #[must_use]
    pub const fn index(self) -> u8 {
        self as u8
    }

    /// Whether this is one of the eight bright colours (palette index `>= 8`).
    #[must_use]
    pub const fn is_bright(self) -> bool {
        self.index() >= 8
    }

    /// The [`BasicColor`] for palette index `index`, or `None` if `index` is
    /// outside `0..=15` (fail closed).
    #[must_use]
    pub const fn from_index(index: u8) -> Option<BasicColor> {
        let color = match index {
            0 => BasicColor::Black,
            1 => BasicColor::Red,
            2 => BasicColor::Green,
            3 => BasicColor::Yellow,
            4 => BasicColor::Blue,
            5 => BasicColor::Magenta,
            6 => BasicColor::Cyan,
            7 => BasicColor::White,
            8 => BasicColor::BrightBlack,
            9 => BasicColor::BrightRed,
            10 => BasicColor::BrightGreen,
            11 => BasicColor::BrightYellow,
            12 => BasicColor::BrightBlue,
            13 => BasicColor::BrightMagenta,
            14 => BasicColor::BrightCyan,
            15 => BasicColor::BrightWhite,
            _ => return None,
        };
        Some(color)
    }
}

/// A colour an SGR attribute can carry.
///
/// [`Color::Default`] is the terminal's configured default (SGR `39` / `49`),
/// distinct from any explicit palette entry.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Color {
    /// The terminal's default foreground/background (SGR `39` / `49`).
    Default,
    /// One of the sixteen ANSI named colours (SGR `30..=37` / `90..=97`,
    /// `40..=47` / `100..=107`).
    Basic(BasicColor),
    /// A 256-colour palette index (SGR `38;5;n` / `48;5;n`).
    Indexed(u8),
    /// A 24-bit truecolour (SGR `38;2;r;g;b` / `48;2;r;g;b`).
    Rgb(u8, u8, u8),
}
