//! Shared SVG image-decoding library (`lib/svg`).
//!
//! SVG is the canonical, scalable **source** format for every WM/desktop
//! graphical asset — cursors, icons, notification glyphs, window-chrome
//! artwork (`AGENTS.md` §10). This crate is the first-party decoder for that
//! SVG-first pipeline: it is one of the curated §16.4 image-decoding shared
//! libraries, and — like the rest of the desktop's parsers — it is rolled in
//! house rather than pulled from an external crate (`AGENTS.md` §2.12), so the
//! trusted computing base does not grow for an asset format.
//!
//! # What it produces
//!
//! [`decode`] turns an SVG byte string into an [`SvgImage`]: a square design
//! grid plus an ordered stack of filled polygon [`SvgLayer`]s (bottom layer
//! first), and an optional pointer hotspot. That is exactly the vector form
//! `lib/cursor`'s `VectorCursor` and `lib/icon`'s `VectorIcon` already
//! rasterise through `lib/raster`'s single supersampled polygon path, so the
//! SVG-first pipeline converts an asset **once** into this fast-draw form and
//! never re-parses SVG on the hot compositing path (`AGENTS.md` §10, §2.2).
//!
//! # Untrusted input
//!
//! On-disk assets under `/System/Graphics` are untrusted (`AGENTS.md` §19.5).
//! [`decode`] is therefore total: it never panics for any byte string, returns
//! a precise [`SvgError`] for anything outside the supported subset, and a
//! caller fails closed to its built-in fallback artwork rather than crashing
//! the compositor (`AGENTS.md` §2.9).
//!
//! # Supported subset
//!
//! A flat `<svg>` document with a square `viewBox="0 0 D D"` (or equal
//! `width`/`height`), whose shapes are `<polygon>`, `<polyline>`, `<rect>`, or
//! `<path>` restricted to the straight-line commands `M`/`L`/`H`/`V`/`Z`.
//! Fills are hex (`#rgb`/`#rrggbb` and their alpha forms), a small set of
//! named colours, or `none`, optionally scaled by `fill-opacity`. Coordinates
//! and the design grid are integers. Curves, arcs, gradients, transforms, and
//! a second sub-path are out of subset and fail closed — richer artwork is
//! built by stacking filled layers, never a second rasterisation path
//! (`AGENTS.md` §2.2).
//!
//! ```
//! let svg = br##"<svg viewBox="0 0 10 10">
//!   <polygon points="0,0 10,0 10,10 0,10" fill="#3070f0"/>
//! </svg>"##;
//! let image = rustos_svg::decode(svg).expect("a square filled polygon");
//! assert_eq!(image.design(), 10);
//! assert_eq!(image.layers().len(), 1);
//! assert_eq!(image.layers()[0].fill.b, 0xf0);
//! ```

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod color;
pub mod document;
pub mod error;
pub mod path;
pub mod xml;

#[cfg(test)]
mod tests;

pub use document::{decode, SvgImage, SvgLayer};
pub use error::SvgError;
