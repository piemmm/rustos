//! Integer screen geometry: points and rectangles.
//!
//! The [`Point`] and [`Rect`] types are defined once in the shared
//! [`tairix_geometry`] crate (`lib/geometry`) so the taskbar and the default
//! apps — which may not depend on the window manager —
//! reuse exactly the same coordinate vocabulary without duplication. This module re-exports them, and the desktop [`Scale`], so the rest
//! of the compositor keeps referring to `crate::geometry::{Point, Rect,
//! Scale}`.

pub use tairix_geometry::{Point, Rect, Scale};
