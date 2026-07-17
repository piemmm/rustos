//! Shared text rasterisation primitives (`lib/font`).
//!
//! This crate is the single home of the system's text rendering: the
//! generated Inconsolata EX + M PLUS 1 Code + Noto Sans Hebrew glyph atlas
//! ([`atlas`], emitted by `cargo xtask font-atlas --write` from the committed
//! SIL OFL 1.1 faces in `assets/`), the Unicode glyph lookup over it
//! ([`glyph`]), and the blitter
//! that draws it onto a `lib/raster` `Surface` ([`font`], behind the
//! default-on `render` feature). Font rendering is one of the curated
//! shared-library classes; like `lib/geometry`, `lib/theme`, and
//! `lib/raster`, it lives in `lib/*` so the taskbar and the default apps can
//! draw text without depending on the window manager.
//!
//! Inconsolata EX remains primary for Latin, Greek, Cyrillic, box drawing,
//! arrows, punctuation, and currency; M PLUS 1 Code fills missing Japanese
//! hiragana, katakana, punctuation, and kanji; Noto Sans Hebrew `ExtraCondensed`
//! fills Hebrew and Yiddish letters, punctuation, and marks. Anything outside
//! the merged repertoire renders U+FFFD, visibly wrong rather than silently
//! dropped. The cell model is one scalar per grid entry: a zero-advance
//! combining mark occupies its own cell, while a wide Japanese bitmap may cover
//! the lead and continuation cells reserved by `rustos_vt::char_width`.
//!
//! The [`atlas`] and [`glyph`] modules are dependency-free `no_std` data and
//! lookup, so a consumer that brings its own blitter — the framebuffer
//! console engine `lib/fbcon`, which draws into device-coherent memory with
//! no allocator — depends on this crate with `default-features = false` and
//! stays `alloc`-free; the `lib/raster`-backed blitter rides the default-on
//! `render` feature (one font definition either way).
//!
//! There is no installed-font machinery yet: a `rustos-theme` font role
//! selects a font by family name under `/System/Fonts`, but no faces are
//! installed, so everything draws with the built-in
//! [`BitmapFont::inconsolata`] face. When installed faces arrive they extend
//! this crate; consumers keep calling `BitmapFont::draw_text`.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod atlas;
#[cfg(feature = "render")]
pub mod font;
pub mod glyph;

#[cfg(test)]
mod tests;

#[cfg(feature = "render")]
pub use font::BitmapFont;
pub use glyph::{lookup, lookup_or_fallback, Glyph};
