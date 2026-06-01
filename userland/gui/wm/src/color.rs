//! Colours and premultiplied-alpha compositing arithmetic.
//!
//! The [`Color`], [`Pixel`], and the premultiplied-alpha algebra are
//! defined once in the shared [`rustos_raster`] crate (`lib/raster`) so the
//! taskbar — which may not depend on the window manager (`AGENTS.md` §17.4)
//! — paints into the same surface type without duplicating the colour
//! algebra (§2.2, §6). This module re-exports them so the rest of the
//! compositor keeps referring to `crate::color::{Color, Pixel}`.

pub use rustos_raster::color::{div255, Color, Pixel};
