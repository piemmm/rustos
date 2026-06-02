//! Shared text rasterisation primitives (`lib/font`).
//!
//! This crate is the single home of the desktop's text rendering: a built-in
//! monospace bitmap font ([`glyphs`]) and the glyph blitter that draws it onto
//! a `lib/raster` [`Surface`] ([`font`]). Font rendering is one of the curated
//! shared-library classes (`AGENTS.md` §16.4); like `lib/geometry`,
//! `lib/theme`, and `lib/raster`, it lives in `lib/*` so the taskbar and the
//! default apps can draw text without depending on the window manager
//! (`AGENTS.md` §6, §17.4).
//!
//! There is no installed-font machinery yet: a `rustos-theme` font role
//! selects a font by family name under `/System/Fonts`, but no faces are
//! installed, so the desktop draws with the built-in [`BitmapFont::mono5x7`]
//! face. When scalable faces arrive they extend this crate; consumers keep
//! calling [`BitmapFont::draw_text`].
//!
//! [`Surface`]: rustos_raster::Surface

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod font;
pub mod glyphs;

#[cfg(test)]
mod tests;

pub use font::BitmapFont;
pub use glyphs::{Glyph, FIRST_CHAR, GLYPH_HEIGHT, GLYPH_WIDTH, LAST_CHAR};
