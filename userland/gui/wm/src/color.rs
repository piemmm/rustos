//! Colours and premultiplied-alpha compositing arithmetic.
//!
//! The [`Color`], [`Pixel`], and the premultiplied-alpha algebra are
//! defined once in the shared [`tairix_raster`] crate (`lib/raster`) so the
//! taskbar — which may not depend on the window manager
//! — paints into the same surface type without duplicating the colour
//! algebra. This module re-exports them so the rest of the
//! compositor keeps referring to `crate::color::{Color, Pixel}`.
//!
//! [`DitherRow`] comes with them because every operator there rounds at a
//! caller-chosen bias: a translucent layer over the wallpaper admits only
//! `256 - a` of the picture's 256 levels, so the composite varies that bias
//! per pixel and spends the missing resolution on the area instead of
//! stepping a smooth backdrop into bands.

pub use tairix_raster::color::{div255, Color, Pixel};
pub use tairix_raster::dither::DitherRow;
