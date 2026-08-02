//! A premultiplied-alpha pixel buffer.
//!
//! A [`Surface`] is the rendered content of one window (or of the
//! compositor's back buffer). It is defined once in the shared
//! [`tairix_raster`] crate (`lib/raster`) so the taskbar — which may not
//! depend on the window manager — paints into the same
//! type without duplication. This module re-exports it so the
//! rest of the compositor keeps referring to `crate::surface::Surface`.

use crate::color::Pixel;

pub use tairix_raster::surface::Surface;

/// Row `y` of `surface` left to right, or an empty slice when the row is out
/// of bounds — a row that does not exist simply draws nothing.
///
/// [`Surface`] exposes a mutable row accessor but not a read-only one, so
/// both the plain-content row sampler and the decorated-window furniture
/// sampler (`crate::window`, `crate::chrome`) share this one row-major index
/// computation rather than each re-deriving it.
pub(crate) fn row(surface: &Surface, y: u32) -> &[Pixel] {
    let width = usize::try_from(surface.width()).unwrap_or(0);
    let Some(start) = usize::try_from(y).ok().and_then(|y| y.checked_mul(width)) else {
        return &[];
    };
    let Some(end) = start.checked_add(width) else {
        return &[];
    };
    surface.pixels().get(start..end).unwrap_or(&[])
}
