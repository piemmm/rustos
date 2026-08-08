//! Shared software rasterisation primitives (`lib/raster`).
//!
//! This crate is the single home of the desktop's premultiplied-alpha
//! colour arithmetic ([`color`]), its CPU pixel buffer ([`surface`]), and
//! the separable box [`blur`] every frosted surface shares — reached as
//! [`Surface::frost_region`], which frosts one rectangle in place through
//! a reusable [`BlurScratch`], for the compositor's window backdrop and a
//! control's selected tile alike.
//! Both the compositing window manager (`userland/gui/wm`) and the
//! taskbar (`userland/gui/taskbar`) draw pixels, but neither may depend
//! on the other; the shared rasteriser therefore
//! lives in `lib/*`, exactly as `lib/geometry` owns the shared
//! coordinate types and `lib/theme` owns the shared design tokens.
//!
//! There is exactly one definition of the colour algebra here, so it is
//! never duplicated into a sibling crate. A theme [`Rgba`] token
//! meets that algebra at a single edge — [`From<Rgba>`](Color) — which
//! is why this crate depends on `lib/theme`: the conversion is owned in
//! one place rather than re-implemented by each consumer.
//!
//! [`Rgba`]: tairix_theme::Rgba

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod blur;
pub mod color;
pub mod resample;
pub mod round;
pub mod surface;

#[cfg(test)]
mod tests;

pub use blur::{box_blur, BlurScratch};
pub use color::{div255, Color, Pixel};
pub use resample::{resample, resample_rows, Region, ResampleError, Rgba8Image};
pub use round::round_rect_coverage;
pub use surface::{Surface, SUBPIXEL};
