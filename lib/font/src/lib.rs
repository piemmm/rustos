//! Shared text rasterisation primitives (`lib/font`).
//!
//! This crate is the system's text-rendering front end. It holds two things
//! and no font outline: the compiled-in **console atlas** ([`atlas`], emitted
//! by `cargo xtask font-atlas --write` from the committed SIL OFL 1.1 primary
//! Inconsolata EX face in `assets/`) with the Unicode glyph lookup over it
//! ([`glyph`]), and — behind the default-on `render` feature — the
//! service-backed blitter ([`font`]) that draws onto a `lib/raster` `Surface`.
//! Font rendering is one of the curated shared-library classes; like
//! `lib/geometry`, `lib/theme`, and `lib/raster`, it lives in `lib/*` so the
//! taskbar and the default apps can draw text without depending on the window
//! manager.
//!
//! # The atlas is the kernel/console subset only
//!
//! The compiled-in atlas is the primary Inconsolata EX face's whole
//! repertoire (Latin, Greek, Cyrillic, box drawing, arrows, punctuation,
//! currency, U+FFFD; §2.4 of `plans/FONT-SERVICE.md`). It is the boot/headless
//! text console's glyph source (`lib/fbcon`, which brings its own allocator-free
//! blitter via `default-features = false`), and it supplies the render path's
//! monospace **geometry constants**. The CJK and Hebrew companion faces are
//! **not** compiled in anywhere: the console falls back to U+FFFD for such a
//! scalar, while rich CJK/Hebrew text is served by `fontd` (below).
//!
//! # Rendering goes through the sandboxed font service
//!
//! With the `render` feature, [`BitmapFont`] is a thin, cached client of the
//! OS font service `fontd` (`plans/FONT-SERVICE.md`): it parses no TrueType
//! and holds no face, and [`BitmapFont::draw_text`] fetches each glyph's 8-bit
//! coverage, and each family's line metrics, from the service over
//! `FONT_ENDPOINT` (see [`client`]) and blits it. `fontd` owns the installed
//! `/System/Fonts` faces and rasterises every family, scalar, and size on
//! demand in a minimum-capability sandbox, so a malformed face faults only
//! that sandbox, never a compositor or terminal. The transport is injected: a
//! program links it with `tairix-font/rt`, a host test installs a mock
//! ([`set_font_transport`]); with no transport a draw composites nothing
//! (fail closed) rather than reaching for local font data.
//!
//! A [`BitmapFont`] names a **family** ([`FamilyKey`]) and renders it at a
//! chosen pixel height in physical pixels. [`BitmapFont::console`] keeps the
//! fixed-pitch built-in family at its native size (what the text console
//! draws), [`BitmapFont::monospace`] renders that same family at any other
//! size, and [`BitmapFont::new`]
//! renders any family at any size — the desktop resolves a comfortable size
//! from the theme's logical font size and the DPI scale. A monospace family
//! shares one advance for every glyph; a proportional family advances each
//! glyph by its own reported width, so [`BitmapFont::advance`],
//! [`BitmapFont::text_width`], and [`BitmapFont::truncate_to_width`] measure
//! through the per-glyph advance either way. The cell model for a monospace
//! grid stays one scalar per grid entry, a wide Japanese or Korean bitmap
//! covering the lead and continuation cells reserved by `tairix_vt::char_width`.
//!
//! # A shared cache declaration for the whole endpoint
//!
//! [`glyph_cache`] (feature `glyph-cache`, pulled in by `render`) declares
//! the one cached-glyph value type, classification, and RAM-derived byte
//! budget both this crate's own client cache and `fontd`'s service-side
//! cache build their bounded cache from, so a glyph cache is declared once
//! rather than separately on each side of the font-service boundary.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(any(feature = "render", feature = "glyph-cache"))]
extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod atlas;
#[cfg(feature = "render")]
pub mod client;
#[cfg(feature = "render")]
pub mod font;
pub mod glyph;
#[cfg(feature = "glyph-cache")]
pub mod glyph_cache;

#[cfg(test)]
mod tests;

#[cfg(feature = "render")]
pub use client::{
    families, set_font_transport, set_glyph_cache, trim_glyph_cache, FontTransport, GlyphCache,
    GlyphKey,
};
#[cfg(feature = "test-util")]
pub use client::{install_test_transport, SolidTestTransport};
#[cfg(feature = "render")]
pub use font::BitmapFont;
pub use glyph::{lookup, lookup_or_fallback, Glyph};
#[cfg(feature = "glyph-cache")]
pub use glyph_cache::{
    glyph_cache_budget, glyph_cache_candidate, CachedGlyph, GLYPH_CACHE_ENTRY_METADATA_BYTES,
};
#[cfg(feature = "render")]
pub use tairix_abi::font_ipc::{FamilyEntry, FamilyKey, FamilyKind, FontMetrics, FontWeight};
