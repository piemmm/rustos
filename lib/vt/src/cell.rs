//! The shared character cell: a glyph plus its rendition [`Attributes`].
//!
//! A [`Cell`] is the unit of screen state both sides of the vocabulary agree
//! on. The terminal emulator's `Grid` stores a [`Cell`] per screen position
//! (the consumer side), and the curses renderer builds the desired screen as
//! [`Cell`]s before diffing it to bytes (the emitter side) — so there is one
//! cell representation, not two (`AGENTS.md` §2.2).

use crate::attr::Attributes;

/// One character cell: the glyph shown at a screen position and the
/// [`Attributes`] it is drawn with.
///
/// An unwritten cell is a space with [`Attributes::PLAIN`], so a screen is
/// always renderable without a separate "empty" sentinel.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Cell {
    /// The glyph shown.
    pub ch: char,
    /// The rendition the glyph is drawn with.
    pub attrs: Attributes,
}

impl Cell {
    /// An unwritten cell: a space with plain attributes.
    pub const BLANK: Self = Self {
        ch: ' ',
        attrs: Attributes::PLAIN,
    };

    /// A cell showing `ch` with plain attributes.
    #[must_use]
    pub const fn plain(ch: char) -> Self {
        Self {
            ch,
            attrs: Attributes::PLAIN,
        }
    }

    /// A cell showing `ch` with the rendition `attrs`.
    #[must_use]
    pub const fn styled(ch: char, attrs: Attributes) -> Self {
        Self { ch, attrs }
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self::BLANK
    }
}
