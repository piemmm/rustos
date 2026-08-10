//! Shared SVG image-decoding library (`lib/svg`).
//!
//! SVG is the canonical, scalable **source** format for every WM/desktop
//! graphical asset — cursors, icons, notification glyphs, window-chrome
//! artwork. This crate is the first-party decoder for that
//! SVG-first pipeline: it is one of the curated image-decoding shared
//! libraries, and — like the rest of the desktop's parsers — it is rolled in
//! house rather than pulled from an external crate, so the
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
//! never re-parses SVG on the hot compositing path.
//!
//! # Untrusted input
//!
//! On-disk assets under `/System/Graphics` are untrusted.
//! [`decode`] is therefore total: it never panics for any byte string, returns
//! a precise [`SvgError`] for anything outside the supported subset, and a
//! caller fails closed to its built-in fallback artwork rather than crashing
//! the compositor.
//!
//! # What it understands
//!
//! The drawable part of SVG 1.1, in full: the document tree (`<g>`, `<defs>`,
//! `<symbol>`, `<use>`, `<switch>`, nested `<svg>` viewports), every basic
//! shape (`<path>`, `<rect>` with rounded corners, `<circle>`, `<ellipse>`,
//! `<line>`, `<polyline>`, `<polygon>`), the whole path grammar including
//! cubic and quadratic curves and elliptical arcs, the whole `transform`
//! grammar, `viewBox` with `preserveAspectRatio`, strokes (width, caps,
//! joins, miter limit, dashes), the presentation-property cascade with the
//! `style` attribute and inheritance, CSS colour syntax with named colours,
//! and linear and radial gradients.
//!
//! It is a *renderer* for artwork, not a browser: text, embedded images,
//! filters, masks, clipping paths, patterns, animation, and scripting are not
//! drawn. An element it cannot draw is skipped rather than refused, so one
//! unsupported decoration does not lose the whole asset; the open question of
//! whether such an element should instead fail the document closed is
//! recorded in `plans/ICONS.md`.
//!
//! ```
//! let svg = br##"<svg viewBox="0 0 10 10">
//!   <circle cx="5" cy="5" r="4" fill="#3070f0" stroke="black" stroke-width="1"/>
//! </svg>"##;
//! let image = tairix_svg::decode(svg).expect("a stroked circle");
//! // The fill, then the stroke over it: SVG's painting order.
//! assert_eq!(image.layers().len(), 2);
//! ```

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod color;
pub mod document;
pub mod error;
pub mod geom;
pub mod number;
pub mod paint;
pub mod pathdata;
pub mod shape;
pub mod stroke;
pub mod style;
pub mod transform;
pub mod xml;

pub use document::{decode, SvgImage, SvgLayer, DESIGN_GRID};
pub use error::SvgError;
