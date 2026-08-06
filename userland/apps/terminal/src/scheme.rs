//! The terminal's colour schemes: the sixteen ANSI colours plus the
//! background, foreground, and cursor a screen is painted with.
//!
//! A [`ColorScheme`] is the one place a terminal colour comes from. The
//! renderer resolves every [`Cell`](tairix_vt::Cell) through it — a
//! [`Color::Default`](tairix_vt::Color) takes the scheme's foreground or
//! background, a [`BasicColor`] takes its slot in [`ColorScheme::ansi`], an
//! indexed colour below sixteen takes that same slot, and the 6×6×6 cube and
//! greyscale ramp above it are the fixed xterm arithmetic no scheme
//! reinterprets.
//!
//! [`Scheme::System`] is the odd one out: it has no colours of its own and
//! resolves against the active desktop [`Theme`], so a terminal left on the
//! default follows the desktop's dark/light appearance exactly as the rest of
//! the session does. Every other built-in carries its own fixed palette, and
//! a user may author one [`Custom`](Scheme::Custom) scheme of their own that
//! is persisted with the rest of their profile.

use tairix_raster::Color;
use tairix_theme::Theme;
use tairix_vt::{Attributes, BasicColor, Color as VtColor};

/// One 24-bit colour of a scheme, as the profile document spells it.
///
/// A bare `rrggbb` triple: the profile grammar cuts a line at its first `#`,
/// so a colour never carries one.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct Rgb {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
}

impl Rgb {
    /// The colour with these channels.
    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Decode the canonical bare `rrggbb` spelling (case-insensitive hex, no
    /// leading `#`); `None` for anything else.
    #[must_use]
    pub fn from_hex(text: &str) -> Option<Self> {
        if text.len() != 6 || !text.is_ascii() {
            return None;
        }
        let r = u8::from_str_radix(text.get(0..2)?, 16).ok()?;
        let g = u8::from_str_radix(text.get(2..4)?, 16).ok()?;
        let b = u8::from_str_radix(text.get(4..6)?, 16).ok()?;
        Some(Self { r, g, b })
    }

    /// Write the canonical lowercase bare `rrggbb` spelling
    /// [`from_hex`](Self::from_hex) reads back into `out`.
    pub fn write_hex(self, out: &mut alloc::string::String) {
        use core::fmt::Write as _;
        let _ = write!(out, "{:02x}{:02x}{:02x}", self.r, self.g, self.b);
    }

    /// This colour as an opaque raster [`Color`].
    #[must_use]
    pub const fn opaque(self) -> Color {
        Color::rgb(self.r, self.g, self.b)
    }

    /// This colour at `alpha`.
    #[must_use]
    pub const fn with_alpha(self, alpha: u8) -> Color {
        Color::rgba(self.r, self.g, self.b, alpha)
    }

    /// The perceived luminance, `0..=255` — the Rec. 601 weighting, in
    /// integer arithmetic.
    #[must_use]
    pub fn luminance(self) -> u8 {
        // The weights total 1000, so the quotient is already within a byte.
        let sum = 299 * u32::from(self.r) + 587 * u32::from(self.g) + 114 * u32::from(self.b);
        u8::try_from(sum / 1000).unwrap_or(u8::MAX)
    }
}

impl From<tairix_theme::Rgba> for Rgb {
    /// A theme colour's visible channels. The theme's own alpha is dropped:
    /// a terminal's translucency is the profile's to decide, not the
    /// palette's.
    fn from(value: tairix_theme::Rgba) -> Self {
        Self::new(value.r, value.g, value.b)
    }
}

/// The number of ANSI colours a scheme carries: the eight normal colours and
/// their eight bright counterparts.
pub const ANSI_COLORS: usize = 16;

/// A scheme's resolved colours: the sixteen ANSI slots plus the three screen
/// roles.
///
/// This is what the renderer actually reads. A named built-in resolves to a
/// fixed one; [`Scheme::System`] resolves against the live desktop theme, so
/// the same terminal follows a dark/light switch without a second palette
/// definition anywhere.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ColorScheme {
    /// The default background — what a cell with no explicit background is
    /// painted with, and the colour translucency fades.
    pub background: Rgb,
    /// The default foreground — what a cell with no explicit foreground draws
    /// its glyph in.
    pub foreground: Rgb,
    /// The block the cursor cell is filled with.
    pub cursor: Rgb,
    /// The glyph colour inside the cursor block.
    pub cursor_text: Rgb,
    /// The sixteen ANSI colours, normal `0..8` then bright `8..16`.
    pub ansi: [Rgb; ANSI_COLORS],
}

/// The xterm ANSI palette every scheme starts from and the classic terminal
/// look uses unaltered.
const XTERM_ANSI: [Rgb; ANSI_COLORS] = [
    Rgb::new(0, 0, 0),
    Rgb::new(205, 0, 0),
    Rgb::new(0, 205, 0),
    Rgb::new(205, 205, 0),
    Rgb::new(0, 0, 238),
    Rgb::new(205, 0, 205),
    Rgb::new(0, 205, 205),
    Rgb::new(229, 229, 229),
    Rgb::new(127, 127, 127),
    Rgb::new(255, 0, 0),
    Rgb::new(0, 255, 0),
    Rgb::new(255, 255, 0),
    Rgb::new(92, 92, 255),
    Rgb::new(255, 0, 255),
    Rgb::new(0, 255, 255),
    Rgb::new(255, 255, 255),
];

impl ColorScheme {
    /// The scheme a [`Scheme::System`] terminal resolves to on `theme`: the
    /// desktop's own surface and text colours over the xterm ANSI palette, so
    /// the terminal reads as part of the session rather than as a foreign
    /// window.
    #[must_use]
    pub fn from_theme(theme: &Theme) -> Self {
        let palette = theme.palette();
        Self {
            background: palette.surface.into(),
            foreground: palette.on_surface.into(),
            cursor: palette.accent.into(),
            cursor_text: palette.on_accent.into(),
            ansi: XTERM_ANSI,
        }
    }

    /// The RGB of one ANSI `index`, wrapping is impossible: the index is
    /// masked into the sixteen slots.
    #[must_use]
    pub const fn ansi(&self, index: u8) -> Rgb {
        self.ansi[(index & 0x0f) as usize]
    }
}

/// A built-in scheme, or the user's own.
///
/// The variants are the closed vocabulary the profile document spells; adding
/// one means adding a variant, its row in [`Scheme::BUILTINS`], and its
/// palette in [`Scheme::palette`], and the compiler then forces every
/// consumer to state what it means.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Scheme {
    /// Follow the desktop theme's own surface, text, and accent colours.
    System,
    /// A deep blue-black scheme with a cool, low-glare palette.
    Midnight,
    /// The classic green-on-black storage-tube look.
    Phosphor,
    /// The classic amber-on-black look.
    Amber,
    /// A muted dark scheme with warm greys and desaturated accents.
    Ember,
    /// A high-contrast dark scheme: pure black behind saturated colours.
    Contrast,
    /// A light scheme: dark ink on warm paper.
    Paper,
    /// The user's own scheme, authored in their profile.
    Custom,
}

impl Scheme {
    /// Every scheme in the order the settings sheet lists them.
    pub const ALL: [Self; 8] = [
        Self::System,
        Self::Midnight,
        Self::Phosphor,
        Self::Amber,
        Self::Ember,
        Self::Contrast,
        Self::Paper,
        Self::Custom,
    ];

    /// Every scheme that carries a fixed palette of its own — everything but
    /// [`System`](Self::System), which follows the theme, and
    /// [`Custom`](Self::Custom), which the user authors.
    pub const BUILTINS: [Self; 5] = [
        Self::Midnight,
        Self::Phosphor,
        Self::Amber,
        Self::Ember,
        Self::Contrast,
    ];

    /// The canonical spelling in a profile document.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Midnight => "midnight",
            Self::Phosphor => "phosphor",
            Self::Amber => "amber",
            Self::Ember => "ember",
            Self::Contrast => "contrast",
            Self::Paper => "paper",
            Self::Custom => "custom",
        }
    }

    /// The human label the settings sheet shows.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Midnight => "Midnight",
            Self::Phosphor => "Phosphor",
            Self::Amber => "Amber",
            Self::Ember => "Ember",
            Self::Contrast => "Contrast",
            Self::Paper => "Paper",
            Self::Custom => "Custom",
        }
    }

    /// Decode a spelling; `None` for anything outside the closed set.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|scheme| scheme.name() == name)
    }

    /// This scheme's fixed palette, or `None` for the two that have none of
    /// their own: [`System`](Self::System) resolves against the desktop theme
    /// and [`Custom`](Self::Custom) against the user's own profile.
    #[must_use]
    pub const fn palette(self) -> Option<ColorScheme> {
        match self {
            Self::System | Self::Custom => None,
            Self::Midnight => Some(MIDNIGHT),
            Self::Phosphor => Some(PHOSPHOR),
            Self::Amber => Some(AMBER),
            Self::Ember => Some(EMBER),
            Self::Contrast => Some(CONTRAST),
            Self::Paper => Some(PAPER),
        }
    }
}

/// A deep blue-black scheme: low glare, cool accents, legible at length.
const MIDNIGHT: ColorScheme = ColorScheme {
    background: Rgb::new(0x0e, 0x14, 0x1b),
    foreground: Rgb::new(0xc8, 0xd3, 0xdd),
    cursor: Rgb::new(0x4f, 0xa8, 0xdc),
    cursor_text: Rgb::new(0x0e, 0x14, 0x1b),
    ansi: [
        Rgb::new(0x1b, 0x24, 0x2e),
        Rgb::new(0xd4, 0x5a, 0x5a),
        Rgb::new(0x6f, 0xba, 0x6f),
        Rgb::new(0xd0, 0xa8, 0x54),
        Rgb::new(0x4f, 0x8f, 0xdc),
        Rgb::new(0xa8, 0x7f, 0xd4),
        Rgb::new(0x54, 0xb0, 0xb8),
        Rgb::new(0xc8, 0xd3, 0xdd),
        Rgb::new(0x40, 0x50, 0x60),
        Rgb::new(0xf0, 0x7c, 0x7c),
        Rgb::new(0x92, 0xdc, 0x92),
        Rgb::new(0xf0, 0xc8, 0x70),
        Rgb::new(0x7c, 0xb4, 0xf0),
        Rgb::new(0xc8, 0xa2, 0xf0),
        Rgb::new(0x78, 0xd4, 0xdc),
        Rgb::new(0xff, 0xff, 0xff),
    ],
};

/// Green on black: the storage-tube look the phosphor effect was named for.
const PHOSPHOR: ColorScheme = ColorScheme {
    background: Rgb::new(0x00, 0x0a, 0x00),
    foreground: Rgb::new(0x4a, 0xf6, 0x26),
    cursor: Rgb::new(0x4a, 0xf6, 0x26),
    cursor_text: Rgb::new(0x00, 0x0a, 0x00),
    ansi: [
        Rgb::new(0x00, 0x14, 0x00),
        Rgb::new(0x1e, 0x9c, 0x18),
        Rgb::new(0x2f, 0xc4, 0x1c),
        Rgb::new(0x3c, 0xdd, 0x20),
        Rgb::new(0x18, 0x86, 0x14),
        Rgb::new(0x27, 0xae, 0x1a),
        Rgb::new(0x35, 0xd0, 0x1e),
        Rgb::new(0x4a, 0xf6, 0x26),
        Rgb::new(0x10, 0x50, 0x10),
        Rgb::new(0x46, 0xd8, 0x24),
        Rgb::new(0x5c, 0xff, 0x38),
        Rgb::new(0x74, 0xff, 0x50),
        Rgb::new(0x30, 0xb4, 0x1e),
        Rgb::new(0x50, 0xe8, 0x30),
        Rgb::new(0x68, 0xff, 0x44),
        Rgb::new(0xc8, 0xff, 0xb4),
    ],
};

/// Amber on black: the other classic monochrome tube.
const AMBER: ColorScheme = ColorScheme {
    background: Rgb::new(0x0d, 0x08, 0x00),
    foreground: Rgb::new(0xff, 0xb0, 0x28),
    cursor: Rgb::new(0xff, 0xb0, 0x28),
    cursor_text: Rgb::new(0x0d, 0x08, 0x00),
    ansi: [
        Rgb::new(0x1a, 0x10, 0x00),
        Rgb::new(0xa8, 0x62, 0x10),
        Rgb::new(0xc4, 0x7c, 0x16),
        Rgb::new(0xdc, 0x94, 0x1c),
        Rgb::new(0x94, 0x54, 0x0c),
        Rgb::new(0xb8, 0x70, 0x14),
        Rgb::new(0xd0, 0x88, 0x1a),
        Rgb::new(0xff, 0xb0, 0x28),
        Rgb::new(0x50, 0x34, 0x08),
        Rgb::new(0xd8, 0x8c, 0x20),
        Rgb::new(0xff, 0xa4, 0x2c),
        Rgb::new(0xff, 0xc0, 0x50),
        Rgb::new(0xbc, 0x74, 0x18),
        Rgb::new(0xe8, 0x98, 0x24),
        Rgb::new(0xff, 0xb8, 0x40),
        Rgb::new(0xff, 0xe0, 0xa8),
    ],
};

/// Warm greys and desaturated accents: dark, but not cold.
const EMBER: ColorScheme = ColorScheme {
    background: Rgb::new(0x1c, 0x18, 0x16),
    foreground: Rgb::new(0xdc, 0xd2, 0xc8),
    cursor: Rgb::new(0xe0, 0x7b, 0x39),
    cursor_text: Rgb::new(0x1c, 0x18, 0x16),
    ansi: [
        Rgb::new(0x2a, 0x25, 0x22),
        Rgb::new(0xcb, 0x60, 0x4c),
        Rgb::new(0x8f, 0xa8, 0x5e),
        Rgb::new(0xd8, 0x9b, 0x44),
        Rgb::new(0x6f, 0x94, 0xb0),
        Rgb::new(0xa8, 0x7c, 0xa0),
        Rgb::new(0x68, 0xa8, 0x9c),
        Rgb::new(0xdc, 0xd2, 0xc8),
        Rgb::new(0x57, 0x4e, 0x48),
        Rgb::new(0xe8, 0x80, 0x6c),
        Rgb::new(0xaf, 0xc8, 0x7c),
        Rgb::new(0xf0, 0xbc, 0x68),
        Rgb::new(0x92, 0xb4, 0xd0),
        Rgb::new(0xc8, 0x9c, 0xc0),
        Rgb::new(0x88, 0xc8, 0xbc),
        Rgb::new(0xf6, 0xf0, 0xe8),
    ],
};

/// Pure black behind saturated colours, for maximum legibility.
const CONTRAST: ColorScheme = ColorScheme {
    background: Rgb::new(0x00, 0x00, 0x00),
    foreground: Rgb::new(0xff, 0xff, 0xff),
    cursor: Rgb::new(0xff, 0xff, 0xff),
    cursor_text: Rgb::new(0x00, 0x00, 0x00),
    ansi: [
        Rgb::new(0x00, 0x00, 0x00),
        Rgb::new(0xff, 0x40, 0x40),
        Rgb::new(0x40, 0xff, 0x40),
        Rgb::new(0xff, 0xff, 0x40),
        Rgb::new(0x60, 0x80, 0xff),
        Rgb::new(0xff, 0x60, 0xff),
        Rgb::new(0x40, 0xff, 0xff),
        Rgb::new(0xe8, 0xe8, 0xe8),
        Rgb::new(0x80, 0x80, 0x80),
        Rgb::new(0xff, 0x80, 0x80),
        Rgb::new(0x80, 0xff, 0x80),
        Rgb::new(0xff, 0xff, 0x80),
        Rgb::new(0xa0, 0xb8, 0xff),
        Rgb::new(0xff, 0xa0, 0xff),
        Rgb::new(0xa0, 0xff, 0xff),
        Rgb::new(0xff, 0xff, 0xff),
    ],
};

/// Dark ink on warm paper: the light scheme.
const PAPER: ColorScheme = ColorScheme {
    background: Rgb::new(0xf7, 0xf3, 0xea),
    foreground: Rgb::new(0x2b, 0x2b, 0x2b),
    cursor: Rgb::new(0x2b, 0x5c, 0x8a),
    cursor_text: Rgb::new(0xf7, 0xf3, 0xea),
    ansi: [
        Rgb::new(0x2b, 0x2b, 0x2b),
        Rgb::new(0xa8, 0x28, 0x28),
        Rgb::new(0x2e, 0x7a, 0x2e),
        Rgb::new(0x8a, 0x6a, 0x14),
        Rgb::new(0x24, 0x50, 0xa0),
        Rgb::new(0x86, 0x30, 0x8a),
        Rgb::new(0x1c, 0x74, 0x7c),
        Rgb::new(0x5c, 0x5c, 0x5c),
        Rgb::new(0x76, 0x76, 0x76),
        Rgb::new(0xc8, 0x3c, 0x3c),
        Rgb::new(0x3c, 0x96, 0x3c),
        Rgb::new(0xa8, 0x86, 0x24),
        Rgb::new(0x38, 0x68, 0xc0),
        Rgb::new(0xa2, 0x44, 0xa8),
        Rgb::new(0x28, 0x90, 0x98),
        Rgb::new(0x1a, 0x1a, 0x1a),
    ],
};

/// The colours a terminal is actually painting with right now: the resolved
/// [`ColorScheme`] plus the background alpha translucency asks for.
///
/// Resolving happens once per repaint, never per cell, so a screenful of
/// cells costs one theme lookup rather than thousands.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Painted {
    /// The resolved colours.
    pub scheme: ColorScheme,
    /// The alpha the default background is filled at — `255` is opaque, and
    /// anything less lets the compositor show what is behind the window.
    pub background_alpha: u8,
}

impl Painted {
    /// The colours `scheme` resolves to on `theme`, with `custom` standing in
    /// for [`Scheme::Custom`], painted at `background_alpha`.
    #[must_use]
    pub fn resolve(
        scheme: Scheme,
        custom: &ColorScheme,
        theme: &Theme,
        background_alpha: u8,
    ) -> Self {
        let resolved = match scheme.palette() {
            Some(palette) => palette,
            None if scheme == Scheme::Custom => *custom,
            None => ColorScheme::from_theme(theme),
        };
        Self {
            scheme: resolved,
            background_alpha,
        }
    }

    /// The default background, at the translucency in force.
    #[must_use]
    pub const fn background(&self) -> Color {
        self.scheme.background.with_alpha(self.background_alpha)
    }

    /// Resolve a cell's [`Attributes`] into its foreground and background,
    /// applying reverse video last so it swaps the resolved pair.
    ///
    /// A background that resolves to the scheme's own default keeps the
    /// translucent alpha; an explicitly-coloured one is opaque, so highlighted
    /// text stays readable through a translucent window.
    #[must_use]
    pub fn cell_colors(&self, attrs: Attributes) -> (Color, Color) {
        let fg = self.resolve_color(
            attrs.foreground,
            attrs.bold,
            self.scheme.foreground.opaque(),
        );
        let bg = match attrs.background {
            VtColor::Default => self.background(),
            other => self.resolve_color(other, false, self.background()),
        };
        if attrs.reverse {
            // A reversed cell is a solid block of the text colour with the
            // glyph punched out of it; neither may let the desktop through,
            // however translucent the window is.
            (opaque(bg), opaque(fg))
        } else {
            (fg, bg)
        }
    }

    /// Resolve one [`VtColor`] against this scheme, falling back to `default`
    /// for [`VtColor::Default`]. A `bold` basic colour is brightened, the
    /// common terminal convention.
    fn resolve_color(&self, color: VtColor, bold: bool, default: Color) -> Color {
        match color {
            VtColor::Default => default,
            VtColor::Basic(basic) => {
                let basic = if bold { brighten(basic) } else { basic };
                self.scheme.ansi(basic.index()).opaque()
            }
            VtColor::Indexed(index) => self.indexed(index),
            VtColor::Rgb(r, g, b) => Color::rgb(r, g, b),
        }
    }

    /// The colour of a 256-palette `index`: `0..=15` are this scheme's own
    /// ANSI slots, `16..=231` the fixed 6×6×6 cube, and `232..=255` the fixed
    /// 24-step greyscale ramp.
    fn indexed(&self, index: u8) -> Color {
        if index < 16 {
            return self.scheme.ansi(index).opaque();
        }
        if index < 232 {
            let offset = u32::from(index - 16);
            return Color::rgb(
                cube_level(offset / 36),
                cube_level((offset / 6) % 6),
                cube_level(offset % 6),
            );
        }
        let level = u8::try_from(u32::from(index - 232) * 10 + 8).unwrap_or(u8::MAX);
        Color::rgb(level, level, level)
    }
}

/// The same colour with its alpha discarded.
const fn opaque(color: Color) -> Color {
    Color::rgb(color.r, color.g, color.b)
}

/// The bright counterpart of a basic colour; an already-bright colour is left
/// unchanged.
fn brighten(basic: BasicColor) -> BasicColor {
    if basic.is_bright() {
        basic
    } else {
        BasicColor::from_index(basic.index() + 8).unwrap_or(basic)
    }
}

/// One channel of the 6×6×6 colour cube: level `0` is black, the rest are
/// `level * 40 + 55`.
fn cube_level(level: u32) -> u8 {
    if level == 0 {
        0
    } else {
        u8::try_from(level * 40 + 55).unwrap_or(u8::MAX)
    }
}

#[cfg(test)]
#[path = "scheme_tests.rs"]
mod tests;
