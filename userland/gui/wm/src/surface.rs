//! A premultiplied-alpha pixel buffer.
//!
//! A [`Surface`] is the rendered content of one window (or of the
//! compositor's back buffer). It is defined once in the shared
//! [`tairix_raster`] crate (`lib/raster`) so the taskbar — which may not
//! depend on the window manager — paints into the same
//! type without duplication. This module re-exports it so the
//! rest of the compositor keeps referring to `crate::surface::Surface`.

pub use tairix_raster::surface::Surface;
