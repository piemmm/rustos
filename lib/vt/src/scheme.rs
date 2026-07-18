//! The standard TAIRiX terminal colour scheme: semantic roles mapped, once, to
//! [`Sgr`] styles.
//!
//! There is exactly one terminal colour scheme in TAIRiX, defined here as data
//! and imported by every consumer (`lib/help`'s renders, `lib/curses`
//! defaults, and each command app) so no tool carries a private colour list.
//! A tool names a [`Role`] — "this text is a directory", "this is an error" —
//! never a raw colour number, so the concrete palette can evolve as data
//! without touching a single call site.
//!
//! # Presentation only
//!
//! Colour and emphasis are presentation, never the sole carrier of a
//! distinction: the information survives with every attribute stripped (a mono
//! terminal, a colourblind reader, a script all see the same facts). So the
//! roles most at risk of confusion under the common colour-vision deficiencies
//! (red-vs-green: [`Role::Error`], [`Role::Success`]) also differ in a text
//! attribute (bold), not hue alone.
//!
//! # Degrades deterministically
//!
//! Each role names its ideal foreground [`Color`]. A renderer degrades that
//! colour to what the terminal can actually show through the one capability
//! judgement in `lib/termcap` / `lib/curses` (truecolour → 256 → 16 → mono);
//! this module holds no depth logic of its own, only the ideal palette. On a
//! monochrome terminal a role degrades to its attribute-only form (or plain),
//! never to garbage.

use crate::attr::Sgr;
use crate::color::{BasicColor, Color};

/// A semantic role in the standard colour scheme.
///
/// Roles name *meaningful distinctions* a human reads — kinds, matches,
/// severities, structure — never decoration. [`Role::style`] maps each to its
/// [`Style`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Role {
    /// A section heading or title.
    Heading,
    /// Emphasised prose (`*emphasis*`).
    Emphasis,
    /// Inline code, a literal, or a fenced block.
    Literal,
    /// A filesystem path or directory name.
    Directory,
    /// An executable file.
    Executable,
    /// A search match or highlighted span.
    Match,
    /// An error message.
    Error,
    /// A warning message.
    Warning,
    /// A success or confirmation message.
    Success,
    /// Secondary / dim metadata.
    Metadata,
    /// A selected item.
    Selection,
    /// A border, rule, or divider.
    Border,
}

impl Role {
    /// Every [`Role`] in declaration order, for exhaustive iteration in tests.
    pub const ALL: [Role; 12] = [
        Role::Heading,
        Role::Emphasis,
        Role::Literal,
        Role::Directory,
        Role::Executable,
        Role::Match,
        Role::Error,
        Role::Warning,
        Role::Success,
        Role::Metadata,
        Role::Selection,
        Role::Border,
    ];

    /// The [`Style`] the standard scheme assigns to this role.
    ///
    /// The colours are the sixteen ANSI palette entries: they render
    /// identically on every colour terminal and need no lossy quantisation,
    /// while a mono terminal drops them through the shared depth judgement.
    /// Where two roles risk confusion under a colour-vision deficiency they
    /// differ in an attribute as well as hue.
    //
    // Two roles may share a style today (heading and directory are both bold
    // bright blue) yet stay distinct concepts a tool names separately and the
    // scheme may recolour independently later, so their arms are kept apart
    // rather than merged.
    #[allow(clippy::match_same_arms)]
    #[must_use]
    pub const fn style(self) -> Style {
        match self {
            // Structure: bright, bold, attention without alarm.
            Role::Heading => Style::new(basic(BasicColor::BrightBlue)).bold(),
            Role::Emphasis => Style::plain().italic(),
            Role::Literal => Style::new(basic(BasicColor::Cyan)),
            // A directory is the classic bold blue; bright blue keeps it
            // readable against a dark background where plain blue is dim.
            Role::Directory => Style::new(basic(BasicColor::BrightBlue)).bold(),
            Role::Executable => Style::new(basic(BasicColor::Green)).bold(),
            Role::Match => Style::new(basic(BasicColor::BrightYellow)).bold(),
            // Error/Success are the red/green pair colour-vision deficiencies
            // confuse, so both also carry bold — the distinction never rests
            // on hue alone.
            Role::Error => Style::new(basic(BasicColor::BrightRed)).bold(),
            Role::Warning => Style::new(basic(BasicColor::Yellow)),
            Role::Success => Style::new(basic(BasicColor::BrightGreen)).bold(),
            Role::Metadata => Style::plain().dim(),
            Role::Selection => Style::new(basic(BasicColor::BrightMagenta)).bold(),
            Role::Border => Style::new(basic(BasicColor::BrightBlack)),
        }
    }
}

/// A `const` [`Color::Basic`] constructor (the enum path is not `const`-usable
/// in a match arm otherwise).
const fn basic(color: BasicColor) -> Color {
    Color::Basic(color)
}

/// The most [`Sgr`] operations one style opens: bold, dim, italic, underline,
/// and the foreground colour.
pub const MAX_STYLE_SGRS: usize = 5;

/// The rendition a role is drawn with: an optional foreground colour and the
/// independent emphasis attributes.
///
/// A [`Style`] is the *ideal* — the colour before the terminal's depth is
/// applied. [`Style::open`] yields the [`Sgr`] operations that turn it on;
/// the caller emits them, prints the text, and resets with [`Sgr::Reset`].
//
// The four flags are independent SGR rendition states, not a state machine:
// any combination is legal, so a flat record models them more clearly than an
// enum — the same rationale `Attributes` documents.
#[allow(clippy::struct_excessive_bools)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Style {
    /// The foreground colour, or [`Color::Default`] for the terminal default.
    pub foreground: Color,
    /// Bold / increased intensity.
    pub bold: bool,
    /// Dim / decreased intensity.
    pub dim: bool,
    /// Italic.
    pub italic: bool,
    /// Underline.
    pub underline: bool,
}

impl Style {
    /// A style with foreground `color` and no attributes.
    #[must_use]
    pub const fn new(color: Color) -> Style {
        Style {
            foreground: color,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
        }
    }

    /// A style with the terminal-default colour and no attributes.
    #[must_use]
    pub const fn plain() -> Style {
        Style::new(Color::Default)
    }

    /// This style with bold set.
    #[must_use]
    pub const fn bold(mut self) -> Style {
        self.bold = true;
        self
    }

    /// This style with dim set.
    #[must_use]
    pub const fn dim(mut self) -> Style {
        self.dim = true;
        self
    }

    /// This style with italic set.
    #[must_use]
    pub const fn italic(mut self) -> Style {
        self.italic = true;
        self
    }

    /// This style with underline set.
    #[must_use]
    pub const fn underline(mut self) -> Style {
        self.underline = true;
        self
    }

    /// Whether this style changes nothing (default colour, no attributes), so a
    /// renderer can skip emitting escape sequences entirely.
    #[must_use]
    pub const fn is_plain(self) -> bool {
        matches!(self.foreground, Color::Default)
            && !self.bold
            && !self.dim
            && !self.italic
            && !self.underline
    }

    /// The [`Sgr`] operations that open this style, in canonical order
    /// (attributes then colour), written into the front of a fixed array.
    ///
    /// Returns the array and the number of operations it holds
    /// (`0..=MAX_STYLE_SGRS`); the caller emits `&array[..count]`. A colour is
    /// emitted verbatim — degrade it to the terminal's depth *before* building
    /// the [`Style`] if needed.
    #[must_use]
    pub fn open(self) -> ([Sgr; MAX_STYLE_SGRS], usize) {
        let mut out = [Sgr::Reset; MAX_STYLE_SGRS];
        let mut n = 0;
        let mut push = |sgr: Sgr| {
            out[n] = sgr;
            n += 1;
        };
        if self.bold {
            push(Sgr::Bold);
        }
        if self.dim {
            push(Sgr::Dim);
        }
        if self.italic {
            push(Sgr::Italic);
        }
        if self.underline {
            push(Sgr::Underline);
        }
        if !matches!(self.foreground, Color::Default) {
            push(Sgr::Foreground(self.foreground));
        }
        (out, n)
    }
}
