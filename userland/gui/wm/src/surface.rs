//! A premultiplied-alpha pixel buffer.
//!
//! A [`Surface`] is the rendered content of one window (or of the
//! compositor's back buffer). It is defined once in the shared
//! [`rustos_raster`] crate (`lib/raster`) so the taskbar — which may not
//! depend on the window manager (`AGENTS.md` §17.4) — paints into the same
//! type without duplication (§2.2, §6). This module re-exports it so the
//! rest of the compositor keeps referring to `crate::surface::Surface`.

pub use rustos_raster::surface::Surface;
