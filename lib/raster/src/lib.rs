//! Shared software rasterisation primitives (`lib/raster`).
//!
//! This crate is the single home of the desktop's premultiplied-alpha
//! colour arithmetic ([`color`]) and its CPU pixel buffer ([`surface`]).
//! Both the compositing window manager (`userland/gui/wm`) and the
//! taskbar (`userland/gui/taskbar`) draw pixels, but neither may depend
//! on the other (`AGENTS.md` §17.4); the shared rasteriser therefore
//! lives in `lib/*` (§6), exactly as `lib/geometry` owns the shared
//! coordinate types and `lib/theme` owns the shared design tokens.
//!
//! There is exactly one definition of the colour algebra here, so it is
//! never duplicated into a sibling crate (§2.2). A theme [`Rgba`] token
//! meets that algebra at a single edge — [`From<Rgba>`](Color) — which
//! is why this crate depends on `lib/theme`: the conversion is owned in
//! one place rather than re-implemented by each consumer.
//!
//! [`Rgba`]: rustos_theme::Rgba

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod color;
pub mod surface;

#[cfg(test)]
mod tests;

pub use color::{div255, Color, Pixel};
pub use surface::Surface;
