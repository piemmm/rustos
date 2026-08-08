//! Shared pointer-cursor library (`lib/cursor`).
//!
//! The desktop's cursors are richer than a one-bit fill mask: each is a
//! small stack of filled, coloured [`Shape`]s over a resolution-independent
//! design grid (a [`VectorCursor`]), so the same definition rasterises
//! crisply at any scale, carries real colour and alpha, and — being pure
//! geometry — is replaceable with an entirely different cursor set. This is
//! the cursor work of `PLAN.md` Stage 7: colourful, scalable, vectorised,
//! and swappable.
//!
//! Like `lib/geometry`, `lib/theme`, `lib/raster`, and `lib/font`, this crate
//! lives in `lib/*` so the window manager and the default apps use it without
//! depending on one another. It owns no colour
//! arithmetic of its own: rasterising a cursor composites through
//! `lib/raster`'s single premultiplied-alpha path, and it
//! names cursors by `lib/theme`'s [`CursorKind`] rather than inventing a
//! second vocabulary.
//!
//! # Pipeline
//!
//! [`CursorTheme`] binds one [`VectorCursor`] to each [`CursorKind`];
//! [`CursorRegistry`] holds the available sets and the active one and lets
//! the running system swap the whole pointer look at runtime. A screen
//! resolves a kind to a cursor, rasterises it at the display scale, and
//! places the resulting [`CursorImage`] as a [`PlacedCursor`], which puts
//! the hotspot on the pointer and samples the artwork for a draw loop.
//!
//! ```
//! use tairix_cursor::{CursorRegistry, CursorImage};
//! use tairix_theme::CursorKind;
//!
//! let cursors = CursorRegistry::with_builtin();
//! let arrow = cursors.active_cursor(CursorKind::Arrow);
//!
//! // Render at native size and at 2× for a high-DPI display.
//! let native: CursorImage = arrow.rasterise(100).expect("renderable");
//! let hidpi: CursorImage = arrow.rasterise(200).expect("renderable");
//! assert_eq!(hidpi.width(), native.width() * 2);
//! ```
//!
//! [`CursorKind`]: tairix_theme::CursorKind

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod load;
pub mod placed;
pub mod raster;
pub mod registry;
pub mod svg;
pub mod theme;
pub mod vector;

#[cfg(test)]
mod tests;

pub use load::{CursorAssetSource, CURSOR_KINDS};
pub use placed::PlacedCursor;
pub use raster::CursorImage;
pub use registry::{CursorRegistry, CursorRegistryError, CursorSetId};
pub use svg::decode as decode_svg;
pub use theme::CursorTheme;
pub use vector::{Shape, VectorCursor, Vertex};
