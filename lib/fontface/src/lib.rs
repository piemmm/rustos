//! Shared TrueType glyph-outline engine (`lib/fontface`).
//!
//! This crate is the single home of TAIRiX's glyph rasterisation: it reads a
//! committed TrueType face ([`Face`]), walks its simple and composite glyph
//! outlines, and fills them into 4-bit (`0..=15`) coverage bitmaps with an
//! anti-aliased non-zero-winding rasteriser — **at any requested pixel size**.
//! [`FontFamily`] layers the merged-repertoire codepoint resolution (an
//! ordered set of scoped faces, earliest-face-wins) on top, so a scalar maps
//! to exactly one face's glyph.
//!
//! Two consumers share this one engine (`AGENTS.md` §2.2):
//!
//! * `cargo xtask font-atlas` rasterises every mapped scalar once, at the
//!   native [`ATLAS_EM_PX`] size, to emit the generated `lib/font` atlas.
//! * `lib/font` embeds the same faces and rasterises a glyph on demand at the
//!   desktop's requested cell height, so UI text is drawn from the outlines at
//!   its true size instead of resampled from a fixed bitmap — crisp whether
//!   tiny or very large.
//!
//! The engine is `no_std` + `alloc` (a rasterised glyph is a heap
//! `Vec<u8>` of coverage) and contains no `unsafe`. It fails closed: any
//! malformed or unsupported table yields a [`FontError`] rather than a wrong
//! glyph or a panic. Floating-point rounding uses the crate's own bounded
//! [`mathf`] helpers so it needs no `std` libm.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

#[cfg(test)]
extern crate std;

mod engine;
mod family;
mod mathf;

#[cfg(test)]
mod tests;

pub use engine::{CellGeometry, Face};
pub use family::{FontFamily, Repertoire};

/// The pixels-per-em the generated `lib/font` atlas is rasterised at.
///
/// The native atlas cell is 15×28 (ascent 23, descent 5); at 25 px/em
/// Inconsolata EX's 613/1024-em advance rounds to a 15-pixel cell with under
/// 0.2% grid distortion. The runtime scales this reference linearly to reach a
/// requested cell height, so both the atlas and any resized glyph share one
/// definition of the native size (`AGENTS.md` §2.2).
pub const ATLAS_EM_PX: u32 = 25;

/// A parse or rasterisation failure.
///
/// The committed faces are trusted repository data, but the engine still fails
/// closed on anything malformed or unsupported rather than emitting a wrong
/// glyph. The message is a static description of what could not be read.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FontError {
    what: &'static str,
}

impl FontError {
    /// A failure describing `what` could not be read or was unsupported.
    #[must_use]
    pub const fn new(what: &'static str) -> Self {
        Self { what }
    }

    /// The static description of the failure.
    #[must_use]
    pub const fn message(self) -> &'static str {
        self.what
    }
}

impl core::fmt::Display for FontError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "malformed or unsupported TrueType data: {}", self.what)
    }
}
