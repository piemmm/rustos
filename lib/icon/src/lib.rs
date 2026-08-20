//! Shared desktop-icon library (`lib/icon`).
//!
//! The desktop's status and notification icons are scalable vector artwork,
//! not fixed-resolution bitmaps: each [`VectorIcon`] is a small stack of
//! filled polygon layers over a resolution-independent design grid, so the
//! same glyph rasterises crisply at any DPI / UI scale and is tinted by the
//! active theme rather than baking a palette into a bitmap. This is the
//! notification-icon artwork of `PLAN.md` Stage 7, and the same SVG-first
//! vector-then-rasterise pipeline the cursors already use.
//!
//! Like `lib/geometry`, `lib/theme`, `lib/raster`, `lib/font`, and
//! `lib/cursor`, this crate lives in `lib/*` so the taskbar uses it without
//! the taskbar and the window manager depending on one another. It owns no scan converter or colour arithmetic of its own:
//! every glyph rasterises through `lib/raster`'s single
//! [`Surface::fill_polygon`] path and its one [`Surface::layered`]
//! composition, exactly as a cursor does — there is no second polygon
//! rasteriser.
//!
//! # Pipeline
//!
//! [`IconKind`] names a status/notification glyph; [`builtin_icon`] turns a
//! kind plus a single theme colour into a [`VectorIcon`]; the taskbar
//! rasterises that icon to a [`Surface`] sized to the notification slot at
//! the active scale and composites it onto the bar.
//!
//! ```
//! use tairix_icon::{builtin_icon, IconKind};
//! use tairix_raster::Color;
//!
//! let icon = builtin_icon(IconKind::Battery, Color::rgb(230, 230, 235));
//! let image = icon.rasterise(16).expect("renderable");
//! assert_eq!(image.width(), 16);
//! assert_eq!(image.height(), 16);
//! // The glyph drew at least one pixel.
//! assert!(image.pixels().iter().any(|p| p.a > 0));
//! ```
//!
//! [`Surface`]: tairix_raster::Surface
//! [`Surface::fill_polygon`]: tairix_raster::Surface::fill_polygon
//! [`Surface::layered`]: tairix_raster::Surface::layered

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod artwork;
pub mod glyph;
pub mod load;
pub mod svg;
pub mod vector;

#[cfg(test)]
mod tests;

pub use artwork::{
    artwork_cache, artwork_kind_for_file, icon_artwork_path, icon_vector_path, render_artwork,
    ArtworkCache, ArtworkKey, ArtworkOutcome, ArtworkRasteriser, ArtworkReader, ArtworkResolver,
    CachedArtwork, IconArtwork, IconArtworkSource, IconRequest, InlineArtwork, NoArtwork, Resolved,
    ARTWORK_ENTRY_METADATA_BYTES, GRAPHICS_DIR, ICONS_DIR, MAX_ARTWORK_BYTES, MAX_ARTWORK_SIDE,
    MIN_ARTWORK_SIDE,
};
pub use glyph::{builtin_icon, disk_icon, IconKind};
pub use load::{IconAssetSource, IconSet, ICON_KINDS};
pub use svg::decode as decode_svg;
pub use vector::{IconLayer, VectorIcon};
